// Throwaway spike agent. Vsock listener that maintains a monotonic counter
// so we can verify state survives CH snapshot/restore cycles.
//
// Protocol: each accepted connection receives one ASCII line:
//   COUNTER=<n> UPTIME_MS=<m>\n
// then closes. Counter increments every 100ms.

#define _GNU_SOURCE
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <linux/vm_sockets.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/reboot.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/sysmacros.h>
#include <sys/time.h>
#include <sys/types.h>
#include <termios.h>
#include <time.h>
#include <unistd.h>

static volatile unsigned long counter = 0;
static struct timespec boot_ts;

// Host CID for hybrid vsock (CH and Firecracker convention).
#define HOST_CID 2
#define DIAL_PORT 9999

static unsigned long now_ms(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    unsigned long b = (unsigned long)boot_ts.tv_sec * 1000UL + boot_ts.tv_nsec / 1000000UL;
    unsigned long n = (unsigned long)ts.tv_sec * 1000UL + ts.tv_nsec / 1000000UL;
    return n - b;
}

static void *ticker(void *arg) {
    (void)arg;
    while (1) {
        struct timespec ts = {.tv_sec = 0, .tv_nsec = 100 * 1000 * 1000};
        nanosleep(&ts, NULL);
        __sync_fetch_and_add(&counter, 1);
    }
    return NULL;
}

// Guest-initiated dialer: connect to host CID 2 port 9999 and stream
// counter lines. On any error, close and retry after 200ms. This tests
// whether guest->host vsock survives snapshot/restore (host-initiated
// confirmed broken in CH #7263).
static void *dialer(void *arg) {
    (void)arg;
    unsigned long attempt = 0;
    while (1) {
        attempt++;
        int s = socket(AF_VSOCK, SOCK_STREAM, 0);
        if (s < 0) {
            fprintf(stderr, "dialer: socket() failed: %s\n", strerror(errno));
            sleep(1); continue;
        }

        struct sockaddr_vm sa = {0};
        sa.svm_family = AF_VSOCK;
        sa.svm_cid = HOST_CID;
        sa.svm_port = DIAL_PORT;

        // Non-blocking connect with 1s timeout, so a wedged device doesn't
        // freeze the dialer thread.
        int flags = fcntl(s, F_GETFL, 0);
        fcntl(s, F_SETFL, flags | O_NONBLOCK);
        int cr = connect(s, (struct sockaddr *)&sa, sizeof(sa));
        if (cr < 0 && errno != EINPROGRESS) {
            fprintf(stderr, "dialer: attempt=%lu connect failed: %s (counter=%lu)\n",
                    attempt, strerror(errno), __sync_fetch_and_add(&counter, 0));
            close(s);
            struct timespec ts = {.tv_sec = 0, .tv_nsec = 200 * 1000 * 1000};
            nanosleep(&ts, NULL);
            continue;
        }
        if (cr < 0) {
            fd_set wfds;
            FD_ZERO(&wfds); FD_SET(s, &wfds);
            struct timeval tv = {.tv_sec = 1, .tv_usec = 0};
            int sr = select(s + 1, NULL, &wfds, NULL, &tv);
            if (sr <= 0) {
                fprintf(stderr, "dialer: attempt=%lu connect TIMEOUT (counter=%lu)\n",
                        attempt, __sync_fetch_and_add(&counter, 0));
                close(s);
                continue;
            }
            int err = 0; socklen_t elen = sizeof(err);
            getsockopt(s, SOL_SOCKET, SO_ERROR, &err, &elen);
            if (err != 0) {
                fprintf(stderr, "dialer: attempt=%lu connect err=%s (counter=%lu)\n",
                        attempt, strerror(err), __sync_fetch_and_add(&counter, 0));
                close(s);
                struct timespec ts = {.tv_sec = 0, .tv_nsec = 200 * 1000 * 1000};
                nanosleep(&ts, NULL);
                continue;
            }
        }
        fcntl(s, F_SETFL, flags);  // back to blocking for write loop
        fprintf(stderr, "dialer: attempt=%lu connected (counter=%lu)\n",
                attempt, __sync_fetch_and_add(&counter, 0));

        // Connected. Stream counter every 200ms until write fails.
        while (1) {
            char buf[128];
            unsigned long n = __sync_fetch_and_add(&counter, 0);
            int len = snprintf(buf, sizeof(buf),
                               "DIAL COUNTER=%lu UPTIME_MS=%lu\n", n, now_ms());
            ssize_t w = write(s, buf, len);
            if (w != len) {
                fprintf(stderr, "dialer: write failed (w=%zd errno=%s), reconnecting\n",
                        w, strerror(errno));
                break;
            }
            struct timespec ts = {.tv_sec = 0, .tv_nsec = 200 * 1000 * 1000};
            nanosleep(&ts, NULL);
        }
        close(s);
    }
    return NULL;
}

// Identify virtio devices by device ID 19 (vsock) and unbind/rebind them
// via sysfs. Driver name is read from the existing driver symlink.
static int rebind_virtio_vsock(void) {
    DIR *d = opendir("/sys/bus/virtio/devices");
    if (!d) {
        fprintf(stderr, "rebind: opendir /sys/bus/virtio/devices: %s\n",
                strerror(errno));
        return -1;
    }
    int rebound = 0;
    struct dirent *e;
    while ((e = readdir(d))) {
        if (strncmp(e->d_name, "virtio", 6) != 0) continue;

        char path[256], buf[64];
        snprintf(path, sizeof(path),
                 "/sys/bus/virtio/devices/%s/device", e->d_name);
        int f = open(path, O_RDONLY);
        if (f < 0) continue;
        ssize_t n = read(f, buf, sizeof(buf) - 1);
        close(f);
        if (n <= 0) continue;
        buf[n] = '\0';
        long dev_id = strtol(buf, NULL, 0);  // hex like 0x13 or decimal
        if (dev_id != 19) continue;

        // Resolve driver name from /sys/.../driver -> ../../../bus/virtio/drivers/<name>
        char drv_link[256], drv_path[256];
        snprintf(drv_link, sizeof(drv_link),
                 "/sys/bus/virtio/devices/%s/driver", e->d_name);
        ssize_t ln = readlink(drv_link, drv_path, sizeof(drv_path) - 1);
        if (ln < 0) {
            fprintf(stderr, "rebind: %s: no driver bound\n", e->d_name);
            continue;
        }
        drv_path[ln] = '\0';
        char *drv_name = strrchr(drv_path, '/');
        drv_name = drv_name ? drv_name + 1 : drv_path;
        fprintf(stderr, "rebind: %s driver=%s -- unbinding\n",
                e->d_name, drv_name);

        char unbind_path[512], bind_path[512];
        snprintf(unbind_path, sizeof(unbind_path),
                 "/sys/bus/virtio/drivers/%s/unbind", drv_name);
        snprintf(bind_path, sizeof(bind_path),
                 "/sys/bus/virtio/drivers/%s/bind", drv_name);

        f = open(unbind_path, O_WRONLY);
        if (f < 0) {
            fprintf(stderr, "rebind: open %s: %s\n", unbind_path, strerror(errno));
            continue;
        }
        if (write(f, e->d_name, strlen(e->d_name)) < 0) {
            fprintf(stderr, "rebind: write unbind: %s\n", strerror(errno));
        }
        close(f);

        struct timespec ts = {.tv_sec = 0, .tv_nsec = 100 * 1000 * 1000};
        nanosleep(&ts, NULL);

        f = open(bind_path, O_WRONLY);
        if (f < 0) {
            fprintf(stderr, "rebind: open %s: %s\n", bind_path, strerror(errno));
            continue;
        }
        if (write(f, e->d_name, strlen(e->d_name)) < 0) {
            fprintf(stderr, "rebind: write bind: %s\n", strerror(errno));
        }
        close(f);

        rebound++;
        fprintf(stderr, "rebind: %s rebound\n", e->d_name);
    }
    closedir(d);
    return rebound;
}

// Brute-force test: rebind virtio_vsock every 5 seconds. If post-restore
// recovery is possible at all, a rebind cycle will eventually trigger it
// and the dialer's next connect() will reach the host.
static void *periodic_rebind(void *arg) {
    (void)arg;
    sleep(2);  // let things settle at boot
    while (1) {
        sleep(5);
        fprintf(stderr, "periodic_rebind: starting cycle (counter=%lu)\n",
                __sync_fetch_and_add(&counter, 0));
        int rebound = rebind_virtio_vsock();
        fprintf(stderr, "periodic_rebind: cycle done, rebound=%d\n", rebound);
    }
    return NULL;
}

// Read /dev/ttyS0 line-by-line; on RESUMED rebind virtio_vsock and reply OK;
// on PING reply PONG. Anything else: ignore.
static void *serial_ctrl(void *arg) {
    (void)arg;

    int fd = open("/dev/ttyS0", O_RDWR | O_NOCTTY);
    if (fd < 0) {
        fprintf(stderr, "serial_ctrl: open /dev/ttyS0: %s\n", strerror(errno));
        return NULL;
    }

    // Configure raw mode at high baud (CH ignores baud but be explicit).
    struct termios tio;
    if (tcgetattr(fd, &tio) == 0) {
        cfmakeraw(&tio);
        cfsetspeed(&tio, B115200);
        tcsetattr(fd, TCSANOW, &tio);
    }

    fprintf(stderr, "serial_ctrl: ready\n");
    char buf[256];
    size_t off = 0;
    while (1) {
        ssize_t r = read(fd, buf + off, sizeof(buf) - 1 - off);
        if (r <= 0) {
            if (errno == EINTR) continue;
            fprintf(stderr, "serial_ctrl: read: %s\n", strerror(errno));
            sleep(1);
            continue;
        }
        off += (size_t)r;
        buf[off] = '\0';

        char *nl;
        while ((nl = memchr(buf, '\n', off))) {
            *nl = '\0';
            // strip trailing \r
            if (nl > buf && nl[-1] == '\r') nl[-1] = '\0';
            char *cmd = buf;
            fprintf(stderr, "serial_ctrl: cmd=%s\n", cmd);

            if (strcmp(cmd, "PING") == 0) {
                dprintf(fd, "PONG\r\n");
            } else if (strcmp(cmd, "RESUMED") == 0) {
                int n = rebind_virtio_vsock();
                dprintf(fd, "OK rebound=%d\r\n", n);
            } else if (strncmp(cmd, "#", 1) == 0 || cmd[0] == '\0') {
                // ignore comments/empty lines
            } else {
                dprintf(fd, "ERR unknown cmd: %s\r\n", cmd);
            }

            // shift remaining bytes to start of buffer
            size_t consumed = (size_t)(nl - buf) + 1;
            if (consumed < off) {
                memmove(buf, nl + 1, off - consumed);
                off -= consumed;
            } else {
                off = 0;
            }
        }
        if (off == sizeof(buf) - 1) {
            // line too long, discard
            fprintf(stderr, "serial_ctrl: line overflow, discarding\n");
            off = 0;
        }
    }
    return NULL;
}

int main(int argc, char **argv) {
    (void)argc; (void)argv;

    // Mount /proc, /sys, /dev. /dev (devtmpfs) is required for /dev/ttyS0;
    // /sys is required for the virtio rebind path.
    mkdir("/proc", 0555);
    mkdir("/sys", 0555);
    mount("proc", "/proc", "proc", 0, NULL);
    mount("sysfs", "/sys", "sysfs", 0, NULL);
    if (mount("devtmpfs", "/dev", "devtmpfs", 0, NULL) < 0 && errno != EBUSY) {
        fprintf(stderr, "main: mount /dev: %s -- falling back to mknod\n",
                strerror(errno));
        mknod("/dev/ttyS0", S_IFCHR | 0600, makedev(4, 64));
    }

    clock_gettime(CLOCK_MONOTONIC, &boot_ts);

    // Reroute stderr to /dev/kmsg so log lines go through printk -> dmesg,
    // which uses a separate path from virtio-console. If virtio-console
    // wedges post-restore but kernel printk still works, we keep visibility
    // (and CH's `console=hvc0` will still capture printk to console.log).
    {
        int km = open("/dev/kmsg", O_WRONLY | O_APPEND);
        if (km >= 0) {
            dup2(km, 2);
            close(km);
            setvbuf(stderr, NULL, _IOLBF, 0);
        }
    }

    // Boot-time inventory: log what's exposed in /sys/devices/virtual/misc
    // so we can confirm whether vmgenid is available to drive recovery.
    {
        DIR *d = opendir("/sys/devices/virtual/misc");
        if (d) {
            struct dirent *e;
            fprintf(stderr, "boot: /sys/devices/virtual/misc:");
            while ((e = readdir(d))) {
                if (e->d_name[0] == '.') continue;
                fprintf(stderr, " %s", e->d_name);
            }
            fprintf(stderr, "\n");
            closedir(d);
        }
        // Also check sysfs class for vmgenid driver
        if (access("/sys/devices/virtual/misc/vmgenid", F_OK) == 0) {
            fprintf(stderr, "boot: vmgenid present\n");
        } else {
            fprintf(stderr, "boot: vmgenid NOT present\n");
        }
    }

    pthread_t tid_t, tid_d, tid_s, tid_r;
    pthread_create(&tid_t, NULL, ticker, NULL);
    pthread_create(&tid_d, NULL, dialer, NULL);
    pthread_create(&tid_s, NULL, serial_ctrl, NULL);
    pthread_create(&tid_r, NULL, periodic_rebind, NULL);

    int s = socket(AF_VSOCK, SOCK_STREAM, 0);
    if (s < 0) {
        perror("socket(AF_VSOCK)");
        return 1;
    }

    struct sockaddr_vm sa = {0};
    sa.svm_family = AF_VSOCK;
    sa.svm_cid = VMADDR_CID_ANY;
    sa.svm_port = 1234;

    if (bind(s, (struct sockaddr *)&sa, sizeof(sa)) < 0) {
        perror("bind");
        return 1;
    }
    if (listen(s, 16) < 0) {
        perror("listen");
        return 1;
    }

    fprintf(stderr, "spike-agent: listening on vsock port 1234\n");

    while (1) {
        int c = accept(s, NULL, NULL);
        if (c < 0) {
            if (errno == EINTR) continue;
            perror("accept");
            continue;
        }
        char buf[128];
        unsigned long n = __sync_fetch_and_add(&counter, 0);
        int len = snprintf(buf, sizeof(buf),
                           "COUNTER=%lu UPTIME_MS=%lu\n", n, now_ms());
        ssize_t off = 0;
        while (off < len) {
            ssize_t w = write(c, buf + off, len - off);
            if (w < 0) {
                if (errno == EINTR) continue;
                break;
            }
            off += w;
        }
        close(c);
    }

    return 0;
}
