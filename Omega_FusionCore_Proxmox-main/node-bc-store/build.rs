// Détection automatique de librados au moment du build.
//
// Si librados est trouvé (via pkg-config ou chemins standards), le cfg
// `ceph_detected` est émis et librados est lié automatiquement.
// Aucune action manuelle n'est requise — le store s'adapte à l'environnement.
//
// Nécessite le paquet de développement (librados-dev / ceph-devel) qui fournit
// le lien symbolique librados.so et le fichier .pc.
fn main() {
    println!("cargo::rustc-check-cfg=cfg(ceph_detected)");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=LIBRADOS_PATH");

    if let Some(lib_dir) = probe_librados() {
        // Ajouter le répertoire dans le chemin de recherche du linker
        println!("cargo:rustc-link-search=native={lib_dir}");
        println!("cargo:rustc-link-lib=rados");
        println!("cargo:rustc-cfg=ceph_detected");
        println!(
            "cargo:warning=librados détecté dans {lib_dir} — backend Ceph activé automatiquement"
        );
    } else {
        println!(
            "cargo:warning=librados absent — backend RAM uniquement \
             (installer librados-dev / ceph-devel sur Proxmox pour activer Ceph automatiquement)"
        );
    }
}

/// Retourne le répertoire contenant librados.so si trouvé, sinon None.
fn probe_librados() -> Option<String> {
    // 1. pkg-config (méthode préférée — nécessite librados-dev)
    if let Ok(out) = std::process::Command::new("pkg-config")
        .args(["--libs-only-L", "librados"])
        .output()
    {
        if out.status.success() {
            let libs = String::from_utf8_lossy(&out.stdout);
            // pkg-config retourne "-L/usr/lib/x86_64-linux-gnu" etc.
            let dir = libs.trim().trim_start_matches("-L").to_string();
            if !dir.is_empty() {
                return Some(dir);
            }
            // pkg-config trouvé mais pas de -L → lib dans le chemin standard
            return Some("/usr/lib".to_string());
        }
    }

    // 2. Variable d'environnement explicite (CI, cross-compilation)
    if let Ok(path) = std::env::var("LIBRADOS_PATH") {
        let p = std::path::Path::new(&path);
        if p.exists() {
            return p
                .parent()
                .map(|d| d.to_string_lossy().into_owned())
                .or(Some(path));
        }
        return None;
    }

    // 3. Chemins standards Debian/Ubuntu/Proxmox/RHEL
    // On cherche librados.so (lien dev) — pas librados.so.2 (runtime seul).
    let candidates = [
        "/usr/lib/librados.so",
        "/usr/lib/x86_64-linux-gnu/librados.so",
        "/usr/lib/aarch64-linux-gnu/librados.so",
        "/usr/lib64/librados.so",
        "/usr/local/lib/librados.so",
    ];
    for path in &candidates {
        if std::path::Path::new(path).exists() {
            let dir = std::path::Path::new(path)
                .parent()
                .map(|d| d.to_string_lossy().into_owned())
                .unwrap_or_else(|| "/usr/lib".to_string());
            return Some(dir);
        }
    }

    None
}
