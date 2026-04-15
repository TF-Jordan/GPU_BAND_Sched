# Makefile — pve_gpu_sched kernel module
# Usage :
#   make -C /lib/modules/$(uname -r)/build M=$(pwd) modules
#   make -C /lib/modules/$(uname -r)/build M=$(pwd) clean

obj-m += pve_gpu_sched.o

pve_gpu_sched-objs := \
    pve_gpu_sched.o      \
    pve_gpu_sched_band.o \
    pve_gpu_sched_mdev.o \
    pve_gpu_sched_bar.o

ccflags-y := -DDEBUG -g
