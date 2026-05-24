/*
 * bridge.c — LD_PRELOAD bridge omega-remote-paging
 *
 * Intercepte mmap() et mmap64() dans QEMU pour :
 *   1. Détecter le mapping MAP_SHARED de /dev/shm/omega-vm-{vmid}
 *      ou du memfd omega-vm-{vmid}-...
 *   2. Créer un userfaultfd enregistré sur la plage QEMU
 *   3. Envoyer le fd + (base, len) à node-a-agent via SCM_RIGHTS
 *
 * Log de diagnostic : /tmp/omega-bridge.log
 *
 * Note: QEMU compilé avec _FILE_OFFSET_BITS=64 appelle mmap64() et non mmap()
 * via PLT — les deux symboles doivent être interceptés.
 */

#define _GNU_SOURCE
#include <dlfcn.h>
#include <sys/mman.h>
#include <sys/types.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <sys/ioctl.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <linux/userfaultfd.h>
#include <fcntl.h>
#include <unistd.h>
#include <string.h>
#include <stdio.h>
#include <stdlib.h>
#include <stdint.h>
#include <stdarg.h>
#include <pthread.h>
#include <errno.h>

/* ── log fichier ──────────────────────────────────────────────────────────── */

static void bridge_log(const char *fmt, ...)
{
    FILE *f = fopen("/tmp/omega-bridge.log", "a");
    if (!f) return;
    struct timeval tv;
    gettimeofday(&tv, NULL);
    fprintf(f, "[%ld.%06ld pid=%d] ", (long)tv.tv_sec, (long)tv.tv_usec, getpid());
    va_list ap;
    va_start(ap, fmt);
    vfprintf(f, fmt, ap);
    va_end(ap);
    fprintf(f, "\n");
    fclose(f);
}

/* ── types réels ──────────────────────────────────────────────────────────── */

/* Sur x86_64, off_t == off64_t == int64_t ; les deux pointeurs ont même ABI */
typedef void *(*mmap_fn)  (void *, size_t, int, int, int, off_t);
typedef void *(*mmap64_fn)(void *, size_t, int, int, int, off64_t);

static mmap_fn   real_mmap   = NULL;
static mmap64_fn real_mmap64 = NULL;

/* ── protection contre la ré-entrance ────────────────────────────────────── */
static __thread int in_bridge = 0;

/* ── dédoublonnage par fd ─────────────────────────────────────────────────── */
#define MAX_REG_FDS 256
static pthread_mutex_t reg_mutex = PTHREAD_MUTEX_INITIALIZER;
static int  registered_fds[MAX_REG_FDS];
static int  registered_fd_count = 0;

static int fd_is_registered(int fd)
{
    pthread_mutex_lock(&reg_mutex);
    for (int i = 0; i < registered_fd_count; i++) {
        if (registered_fds[i] == fd) {
            pthread_mutex_unlock(&reg_mutex);
            return 1;
        }
    }
    pthread_mutex_unlock(&reg_mutex);
    return 0;
}

static void fd_mark_registered(int fd)
{
    pthread_mutex_lock(&reg_mutex);
    if (registered_fd_count < MAX_REG_FDS)
        registered_fds[registered_fd_count++] = fd;
    pthread_mutex_unlock(&reg_mutex);
}

/* ── initialisation ───────────────────────────────────────────────────────── */

static void __attribute__((constructor)) bridge_init(void)
{
    real_mmap   = (mmap_fn)  dlsym(RTLD_NEXT, "mmap");
    real_mmap64 = (mmap64_fn)dlsym(RTLD_NEXT, "mmap64");
    bridge_log("bridge chargé — real_mmap=%p real_mmap64=%p",
               (void *)real_mmap, (void *)real_mmap64);
    if (!real_mmap)
        bridge_log("FATAL: dlsym(mmap) échoué");
    if (!real_mmap64)
        bridge_log("WARN: dlsym(mmap64) échoué (glibc trop ancienne?)");
}

/* ── envoi du fd uffd à node-a-agent ─────────────────────────────────────── */

static int send_uffd_to_agent(int uffd_fd, uint64_t base, uint64_t len, uint32_t vmid)
{
    const char *run_dir = getenv("OMEGA_RUN_DIR");
    if (!run_dir)
        run_dir = "/var/lib/omega-qemu";

    char sock_path[256];
    snprintf(sock_path, sizeof(sock_path), "%s/vm-%u/uffd.sock", run_dir, vmid);

    bridge_log("connexion au socket %s", sock_path);

    int sock = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0);
    if (sock < 0) {
        bridge_log("socket: %s", strerror(errno));
        return -1;
    }

    struct sockaddr_un sa;
    memset(&sa, 0, sizeof(sa));
    sa.sun_family = AF_UNIX;
    memcpy(sa.sun_path, sock_path, strlen(sock_path) + 1);

    if (connect(sock, (struct sockaddr *)&sa, sizeof(sa)) < 0) {
        bridge_log("connect(%s): %s", sock_path, strerror(errno));
        close(sock);
        return -1;
    }

    uint64_t payload[2] = { base, len };

    char cmsg_buf[CMSG_SPACE(sizeof(int))];
    memset(cmsg_buf, 0, sizeof(cmsg_buf));

    struct iovec iov;
    iov.iov_base = payload;
    iov.iov_len  = sizeof(payload);

    struct msghdr msg;
    memset(&msg, 0, sizeof(msg));
    msg.msg_iov        = &iov;
    msg.msg_iovlen     = 1;
    msg.msg_control    = cmsg_buf;
    msg.msg_controllen = sizeof(cmsg_buf);

    struct cmsghdr *cmsg = CMSG_FIRSTHDR(&msg);
    cmsg->cmsg_level = SOL_SOCKET;
    cmsg->cmsg_type  = SCM_RIGHTS;
    cmsg->cmsg_len   = CMSG_LEN(sizeof(int));
    memcpy(CMSG_DATA(cmsg), &uffd_fd, sizeof(int));

    ssize_t sent = sendmsg(sock, &msg, 0);
    close(sock);

    if (sent < 0) {
        bridge_log("sendmsg: %s", strerror(errno));
        return -1;
    }

    bridge_log("OK: uffd fd=%d → %s  base=0x%lx len=%lu vmid=%u",
               uffd_fd, sock_path,
               (unsigned long)base, (unsigned long)len, vmid);
    return 0;
}

/* ── logique d'interception commune ──────────────────────────────────────── */
/*
 * Appelé après que real_mmap (ou real_mmap64) a retourné result.
 * Si le mapping cible /dev/shm/omega-vm-{vmid} ou memfd omega-vm-{vmid}-...,
 * crée et envoie l'uffd.
 * Doit être appelé hors in_bridge (le caller le gère).
 */
static void omega_intercept(void *result, size_t len, int flags, int fd,
                             const char *caller)
{
    /* Log tous les mmap fd-backed ≥ 1 MiB pour diagnostic */
    if (!in_bridge && fd >= 0 && len >= (1UL * 1024 * 1024)) {
        bridge_log("%s: fd=%d len=%lu flags=0x%x result=%p",
                   caller, fd, (unsigned long)len, flags, result);
    }

    if (result == MAP_FAILED
        || fd < 0
        || !(flags & MAP_SHARED)
        || in_bridge
        || len < (1UL * 1024 * 1024))
    {
        return;
    }

    in_bridge = 1;

    char proc_path[64], real_path[256];
    snprintf(proc_path, sizeof(proc_path), "/proc/self/fd/%d", fd);
    ssize_t n = readlink(proc_path, real_path, sizeof(real_path) - 1);

    in_bridge = 0;

    if (n <= 0) {
        bridge_log("readlink(%s) échoué: %s", proc_path, strerror(errno));
        return;
    }
    real_path[n] = '\0';

    bridge_log("MAP_SHARED ≥1MiB [%s]: fd=%d path=%s len=%lu flags=0x%x",
               caller, fd, real_path, (unsigned long)len, flags);

    const char *vmid_start = NULL;
    const char *prefix_shm = "/dev/shm/omega-vm-";
    const char *prefix_memfd = "/memfd:omega-vm-";
    size_t prefix_shm_len = strlen(prefix_shm);
    size_t prefix_memfd_len = strlen(prefix_memfd);

    if (strncmp(real_path, prefix_shm, prefix_shm_len) == 0) {
        vmid_start = real_path + prefix_shm_len;
    } else if (strncmp(real_path, prefix_memfd, prefix_memfd_len) == 0) {
        vmid_start = real_path + prefix_memfd_len;
    } else {
        return;
    }

    if (fd_is_registered(fd)) {
        bridge_log("fd=%d déjà enregistré — ignoré", fd);
        return;
    }

    char *endptr;
    unsigned long vmid = strtoul(vmid_start, &endptr, 10);
    if (endptr == vmid_start || (*endptr != '\0' && *endptr != '-' && *endptr != ' ')) {
        bridge_log("vmid non parseable depuis '%s'", real_path);
        return;
    }

    bridge_log("omega-vm détecté [%s]: vmid=%lu base=%p len=%lu",
               caller, vmid, result, (unsigned long)len);

    int uffd_fd = (int)syscall(SYS_userfaultfd, O_CLOEXEC | O_NONBLOCK);
    if (uffd_fd < 0) {
        bridge_log("userfaultfd: %s", strerror(errno));
        return;
    }

    struct uffdio_api api;
    memset(&api, 0, sizeof(api));
    api.api = UFFD_API;
    if (ioctl(uffd_fd, UFFDIO_API, &api) < 0) {
        bridge_log("UFFDIO_API: %s", strerror(errno));
        close(uffd_fd);
        return;
    }

    struct uffdio_register reg;
    memset(&reg, 0, sizeof(reg));
    reg.range.start = (uint64_t)(uintptr_t)result;
    reg.range.len   = (uint64_t)len;
    reg.mode        = UFFDIO_REGISTER_MODE_MISSING;
    if (ioctl(uffd_fd, UFFDIO_REGISTER, &reg) < 0) {
        bridge_log("UFFDIO_REGISTER: %s", strerror(errno));
        close(uffd_fd);
        return;
    }

    if (send_uffd_to_agent(uffd_fd,
                           (uint64_t)(uintptr_t)result,
                           (uint64_t)len,
                           (uint32_t)vmid) < 0)
    {
        struct uffdio_range range;
        range.start = (uint64_t)(uintptr_t)result;
        range.len   = (uint64_t)len;
        ioctl(uffd_fd, UFFDIO_UNREGISTER, &range);
        close(uffd_fd);
        return;
    }

    fd_mark_registered(fd);

    /* Garder uffd_fd ouvert dans QEMU : le kernel continue à livrer les fautes */
}

/* ── hook mmap ────────────────────────────────────────────────────────────── */

__attribute__((visibility("default")))
void *mmap(void *addr, size_t len, int prot, int flags, int fd, off_t offset)
{
    if (!real_mmap)
        bridge_init();

    void *result = real_mmap(addr, len, prot, flags, fd, offset);
    omega_intercept(result, len, flags, fd, "mmap");
    return result;
}

/* ── hook mmap64 (QEMU compilé avec _FILE_OFFSET_BITS=64 appelle ce symbole) */

__attribute__((visibility("default")))
void *mmap64(void *addr, size_t len, int prot, int flags, int fd, off64_t offset)
{
    if (!real_mmap64) {
        bridge_init();
        /* Fallback si mmap64 indisponible dans glibc suivant */
        if (!real_mmap64)
            return mmap(addr, len, prot, flags, fd, (off_t)offset);
    }

    void *result = real_mmap64(addr, len, prot, flags, fd, offset);
    omega_intercept(result, len, flags, fd, "mmap64");
    return result;
}
