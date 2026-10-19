-- Raw Linux syscalls, for a guest with no userspace to run commands in.
--
-- A kernel-only VM has no shell and no coreutils: the agent's own file
-- operations cover most of what a test needs, and everything else is a
-- syscall issued directly. This module is the syscall half — numbers,
-- struct layouts and errno names, in one place so a test reads as the
-- operation it is performing rather than as a calling convention.
--
-- x86_64 numbers and layouts throughout. Another architecture would
-- need its own table; nothing else here would change.

local M = {}

-- Syscall numbers.
M.NR = {
    getpid = 39, gettid = 186, pidfd_open = 434,
    mount      = 165,
    umount2    = 166,
    newfstatat = 262,
    symlinkat  = 266,
    fchownat   = 260,
    setresgid  = 119,
    setresuid  = 117,
    statfs     = 137,
    openat     = 257,
    close      = 3,
    ioctl      = 16,
    write      = 1,
    read       = 0,
    flock      = 73,
    nanosleep  = 35,
    fsync      = 74,
    unshare    = 272,
    mkdir      = 83,
    getuid     = 102,
    fchmodat   = 268,
    readlinkat = 267,
    getdents64 = 217,
    lseek      = 8,
    ftruncate  = 77,
    utimensat  = 280,
    setxattr   = 188,
    getxattr   = 191,
    removexattr = 197,
    fallocate  = 285,
    splice     = 275,
    copy_file_range = 326,
    mmap       = 9,
    mprotect   = 10,
    munmap     = 11,
    mknodat    = 259,
    pipe2      = 293,
    linkat     = 265,
    unlinkat   = 263,
    renameat2  = 316,
    futex      = 202,
    poll       = 7,
    sched_setaffinity = 203,
    fgetxattr  = 193,
    listxattr  = 194,
    flistxattr = 196,  -- 195 is llistxattr
    quotactl_fd = 443,
}

-- quotactl commands. QCMD packs the command and the quota type into
-- one argument: (cmd << 8) | type.
M.Q_GETQUOTA, M.USRQUOTA, M.GRPQUOTA = 0x800007, 0, 1
function M.QCMD(cmd, qtype) return (cmd << 8) | (qtype & 0xFF) end

-- mmap(2) protections and flags.
M.PROT = { NONE = 0, READ = 1, WRITE = 2 }
M.MAP  = { SHARED = 0x01, PRIVATE = 0x02, ANONYMOUS = 0x20 }

-- File type bits, for mknod.
M.S_IFIFO, M.S_IFCHR, M.S_IFSOCK, M.S_IFREG = 0x1000, 0x2000, 0xC000, 0x8000

-- unlinkat / renameat2 flags.
M.AT_REMOVEDIR = 0x200
M.RENAME_NOREPLACE, M.RENAME_EXCHANGE, M.RENAME_WHITEOUT = 1, 2, 4

-- d_type values a directory entry may carry.
M.DT = { UNKNOWN = 0, FIFO = 1, CHR = 2, DIR = 4, BLK = 6, REG = 8, LNK = 10,
         SOCK = 12 }

-- Namespace flags, for the cases about what a mounter must be
-- entitled to.
M.CLONE_NEWNS   = 0x00020000
M.CLONE_NEWUSER = 0x10000000

-- flock(2) operations.
M.LOCK_SH, M.LOCK_EX, M.LOCK_UN, M.LOCK_NB = 1, 2, 8, 4

-- Flags a test is likely to name.
M.AT_FDCWD            = -100
M.AT_SYMLINK_NOFOLLOW = 0x100
M.AT_EMPTY_PATH       = 0x1000
M.MS_RDONLY           = 1
M.MS_BIND             = 4096
M.MS_REMOUNT          = 32

-- open(2) flags.
M.O = {
    RDONLY = 0, WRONLY = 1, RDWR = 2, CREAT = 0x40, EXCL = 0x80,
    TRUNC = 0x200, APPEND = 0x400, DIRECT = 0x4000, DIRECTORY = 0x10000,
    NOFOLLOW = 0x20000, PATH = 0x200000, TMPFILE = 0x410000,
}

-- The inode flag ioctls, and the one flag stratafs's
-- accepts-modification predicate reads (§4.2.1).
M.FS_IOC_GETFLAGS = 0x80086601
M.FS_IOC_SETFLAGS = 0x40086602
M.FS_IMMUTABLE_FL = 0x00000010

-- Errnos, by the name the TRM uses for them.
M.E = {
    PERM = 1, NOENT = 2, IO = 5, BADF = 9, AGAIN = 11, NOMEM = 12,
    ACCES = 13,
    EXIST = 17, XDEV = 18, NODEV = 19, NOTDIR = 20, ISDIR = 21,
    INVAL = 22, FAULT = 14, ROFS = 30, NOTEMPTY = 39, LOOP = 40, STALE = 116,
    NODATA = 61, RANGE = 34, OPNOTSUPP = 95, NOTTY = 25,
    NAMETOOLONG = 36, NOSPC = 28, MLINK = 31, TXTBSY = 26,
    SRCH = 3, CHILD = 10, BUSY = 16, NOSYS = 38, NOTSOCK = 88, NOTCONN = 107,
    OVERFLOW = 75, BIG2 = 7, IDRM = 43, ESRCH = 3,
    INTR = 4, ADDRINUSE = 98, NETUNREACH = 101, CONNRESET = 104,
    TIMEDOUT = 110, CONNREFUSED = 111, HOSTUNREACH = 113, INPROGRESS = 115,
}

local NAME_OF = {}
for name, value in pairs(M.E) do NAME_OF[value] = name end

--- The name of an errno, for an assertion message: `ENOTEMPTY (39)`.
function M.errname(errno)
    if errno == 0 then return "success" end
    local name = NAME_OF[errno]
    return name and ("E" .. name .. " (" .. errno .. ")") or ("errno " .. errno)
end

--- A NUL-terminated byte string, as every path argument must be.
function M.cstr(s) return s .. "\0" end

-- struct stat, x86_64. Offsets are fixed by the ABI.
local STAT_SIZE = 144
local STAT = {
    dev = 1, ino = 9, nlink = 17, mode = 25, uid = 29, gid = 33,
    rdev = 41, size = 49,
    atime = 73, mtime = 89, ctime = 105,
}

--- stat(2) a path, returning the fields a stratafs test asserts on.
---
--- Returns `nil, errno` on failure, so a caller can assert on either
--- the fields or the refusal without a second call shape.
---
--- `opts.follow` defaults true; false uses AT_SYMLINK_NOFOLLOW, which
--- is how a test tells a symlink from what it points at.
function M.stat(vm, path, opts)
    opts = opts or {}
    local flags = (opts.follow == false) and M.AT_SYMLINK_NOFOLLOW or 0
    local r = vm:syscall(M.NR.newfstatat, {
        args = { M.AT_FDCWD, 0, 0, flags },
        bufs = { M.cstr(path), string.rep("\0", STAT_SIZE) },
        ptrs = { 1, 2 },
    })
    if r.ret ~= 0 then return nil, r.errno end
    return M.decode_stat(r.out_bufs[2])
end

--- Decode a struct stat buffer into the fields a stratafs test asserts on.
function M.decode_stat(buf)
    local mode = string.unpack("<I4", buf, STAT.mode)
    return {
        dev   = string.unpack("<I8", buf, STAT.dev),
        ino   = string.unpack("<I8", buf, STAT.ino),
        nlink = string.unpack("<I8", buf, STAT.nlink),
        mode  = mode,
        perm  = mode & 0xFFF,
        uid   = string.unpack("<I4", buf, STAT.uid),
        gid   = string.unpack("<I4", buf, STAT.gid),
        size  = string.unpack("<i8", buf, STAT.size),
        -- Seconds only: the copy-up cases compare whole timestamps,
        -- and nanoseconds add nothing but noise to the message.
        atime = string.unpack("<i8", buf, STAT.atime),
        mtime = string.unpack("<i8", buf, STAT.mtime),
        ctime = string.unpack("<i8", buf, STAT.ctime),
        -- S_IFMT is the top four bits of the type field.
        is_dir     = (mode & 0xF000) == 0x4000,
        is_symlink = (mode & 0xF000) == 0xA000,
        is_file    = (mode & 0xF000) == 0x8000,
    }
end

-- struct statfs, x86_64. 120 bytes; every field is 8 wide.
local STATFS_SIZE = 120
local STATFS = { type = 1, bsize = 9, namelen = 65, flags = 81 }

--- statfs(2), returning the fields that identify a superblock.
---
--- `type` is the filesystem magic — for stratafs, §4.A's `STRATAFS_MAGIC`.
--- Returns `nil, errno` on failure.
function M.statfs(vm, path)
    local r = vm:syscall(M.NR.statfs, {
        args = { 0, 0 },
        bufs = { M.cstr(path), string.rep("\0", STATFS_SIZE) },
        ptrs = { 0, 1 },
    })
    if r.ret ~= 0 then return nil, r.errno end
    local buf = r.out_bufs[2]
    return {
        type    = string.unpack("<I8", buf, STATFS.type),
        bsize   = string.unpack("<I8", buf, STATFS.bsize),
        namelen = string.unpack("<I8", buf, STATFS.namelen),
        flags   = string.unpack("<I8", buf, STATFS.flags),
    }
end

--- fstat(2), as newfstatat(fd, "", AT_EMPTY_PATH).
---
--- What a descriptor says about itself, which after a copy-up is not
--- what the path says (§4.4.3).
function M.fstat(vm, fd)
    local buf = string.rep("\0", 144)
    local r = vm:syscall(M.NR.newfstatat, {
        args = { fd, 0, 0, M.AT_EMPTY_PATH },
        bufs = { M.cstr(""), buf },
        ptrs = { 1, 2 },
    })
    if r.ret ~= 0 then return nil, r.errno end
    return M.decode_stat(r.out_bufs[2])
end

--- mount(2). Every argument is optional but `target`.
function M.mount(vm, spec)
    return vm:syscall(M.NR.mount, {
        args = { 0, 0, 0, spec.flags or 0, 0 },
        bufs = {
            M.cstr(spec.source or "none"),
            M.cstr(spec.target),
            M.cstr(spec.fstype or ""),
            M.cstr(spec.data or ""),
        },
        ptrs = { 0, 1, 2, 4 },
    })
end

--- umount2(2).
function M.umount(vm, target, flags)
    return vm:syscall(M.NR.umount2, {
        args = { 0, flags or 0 },
        bufs = { M.cstr(target) },
        ptrs = { 0 },
    })
end

--- symlink(2), as symlinkat(AT_FDCWD).
function M.symlink(vm, target, linkpath)
    return vm:syscall(M.NR.symlinkat, {
        args = { 0, M.AT_FDCWD, 0 },
        bufs = { M.cstr(target), M.cstr(linkpath) },
        ptrs = { 0, 2 },
    })
end

--- getdents64(2) against an open directory fd.
---
--- Returns a list of `{ino, off, type, name}` in the order the kernel
--- reported them, or `nil, errno`. An empty list means end of
--- directory. Unlike `vm:listdir` this keeps the descriptor, which is
--- what the capture-at-open cases need, and it reports `.` and `..`,
--- the inode numbers and the offsets, which they also need.
function M.getdents(vm, fd, size)
    size = size or 32768
    local r = vm:syscall(M.NR.getdents64, {
        args = { fd, 0, size },
        bufs = { string.rep("\0", size) },
        ptrs = { 1 },
    })
    if r.ret < 0 then return nil, r.errno end
    local buf, out, at = r.out_bufs[1], {}, 1
    while at <= r.ret do
        local ino = string.unpack("<I8", buf, at)
        local off = string.unpack("<i8", buf, at + 8)
        local reclen = string.unpack("<I2", buf, at + 16)
        local dtype = string.unpack("<I1", buf, at + 18)
        local name = buf:sub(at + 19, at + reclen - 1):match("^[^\0]*")
        out[#out + 1] = { ino = ino, off = off, type = dtype, name = name }
        at = at + reclen
    end
    return out
end

--- Every entry of an open directory, across as many getdents64 calls
--- as it takes. Returns `nil, errno` if any of them fails.
function M.getdents_all(vm, fd, size)
    local all = {}
    while true do
        local batch, errno = M.getdents(vm, fd, size)
        if not batch then return nil, errno end
        if #batch == 0 then return all end
        for _, e in ipairs(batch) do all[#all + 1] = e end
    end
end

--- ftruncate(2).
function M.ftruncate(vm, fd, length)
    return vm:syscall(M.NR.ftruncate, fd, length)
end

--- utimensat(2) on a path, setting access and modification times to a
--- fixed second. `opts.follow = false` sets them on a symlink itself.
function M.utimes(vm, path, seconds, opts)
    local flags = (opts and opts.follow == false) and M.AT_SYMLINK_NOFOLLOW or 0
    -- struct timespec[2], 32 bytes: {sec, nsec} twice.
    local times = string.pack("<i8i8i8i8", seconds, 0, seconds, 0)
    return vm:syscall(M.NR.utimensat, {
        args = { M.AT_FDCWD, 0, 0, flags },
        bufs = { M.cstr(path), times },
        ptrs = { 1, 2 },
    })
end

--- setxattr(2).
function M.setxattr(vm, path, name, value, flags)
    return vm:syscall(M.NR.setxattr, {
        args = { 0, 0, 0, #value, flags or 0 },
        bufs = { M.cstr(path), M.cstr(name), value },
        ptrs = { 0, 1, 2 },
    })
end

--- getxattr(2). Returns the value, or `nil, errno`.
function M.getxattr(vm, path, name, size)
    size = size or 4096
    local r = vm:syscall(M.NR.getxattr, {
        args = { 0, 0, 0, size },
        bufs = { M.cstr(path), M.cstr(name), string.rep("\0", size) },
        ptrs = { 0, 1, 2 },
    })
    if r.ret < 0 then return nil, r.errno end
    return r.out_bufs[3]:sub(1, r.ret)
end

--- fgetxattr(2), against an open descriptor. Returns the value, the
--- required size where `size` is 0, or `nil, errno`.
function M.fgetxattr(vm, fd, name, size)
    size = size or 4096
    local r = vm:syscall(M.NR.fgetxattr, {
        args = { fd, 0, 0, size },
        bufs = { M.cstr(name), string.rep("\0", math.max(size, 1)) },
        ptrs = { 1, 2 },
    })
    if r.ret < 0 then return nil, r.errno end
    if size == 0 then return r.ret end
    return r.out_bufs[2]:sub(1, r.ret)
end

-- struct if_dqblk, 72 bytes. 1-based offsets.
local DQBLK = { curspace = 17, curinodes = 41 }

--- quotactl_fd(2) Q_GETQUOTA against an open fd on the filesystem.
---
--- `quotactl` names a block device, which a tmpfs does not have;
--- `quotactl_fd` takes a descriptor instead, which is the only form
--- that works for the filesystems reachable here.
---
--- Returns `{curspace, curinodes}`, or `nil, errno`.
function M.getquota(vm, fd, id, qtype)
    local r = vm:syscall(M.NR.quotactl_fd, {
        args = { fd, M.QCMD(M.Q_GETQUOTA, qtype or M.USRQUOTA), id, 0 },
        bufs = { string.rep("\0", 72) },
        ptrs = { 3 },
    })
    if r.ret ~= 0 then return nil, r.errno end
    return {
        curspace = string.unpack("<I8", r.out_bufs[1], DQBLK.curspace),
        curinodes = string.unpack("<I8", r.out_bufs[1], DQBLK.curinodes),
    }
end

--- listxattr(2). Returns the names as a list, or `nil, errno`.
function M.listxattr(vm, path, size)
    size = size or 4096
    local r = vm:syscall(M.NR.listxattr, {
        args = { 0, 0, size },
        bufs = { M.cstr(path), string.rep("\0", size) },
        ptrs = { 0, 1 },
    })
    if r.ret < 0 then return nil, r.errno end
    local out = {}
    -- bufs[1] is the path; the names come back in bufs[2].
    for name in r.out_bufs[2]:sub(1, r.ret):gmatch("[^\0]+") do
        out[#out + 1] = name
    end
    return out
end

--- getxattr(2) asking only for the size it would need.
function M.getxattr_size(vm, path, name)
    local r = vm:syscall(M.NR.getxattr, {
        args = { 0, 0, 0, 0 },
        bufs = { M.cstr(path), M.cstr(name) },
        ptrs = { 0, 1 },
    })
    if r.ret < 0 then return nil, r.errno end
    return r.ret
end

--- removexattr(2).
function M.removexattr(vm, path, name)
    return vm:syscall(M.NR.removexattr, {
        args = { 0, 0 },
        bufs = { M.cstr(path), M.cstr(name) },
        ptrs = { 0, 1 },
    })
end

--- fallocate(2).
function M.fallocate(vm, fd, mode, offset, length)
    return vm:syscall(M.NR.fallocate, fd, mode or 0, offset or 0, length or 4096)
end

--- pipe2(2). Returns the read and write fds, or `nil, errno`.
function M.pipe(vm)
    local r = vm:syscall(M.NR.pipe2, {
        args = { 0, 0 },
        bufs = { string.rep("\0", 8) },
        ptrs = { 0 },
    })
    if r.ret ~= 0 then return nil, r.errno end
    local rd, wr = string.unpack("<i4i4", r.out_bufs[1])
    return rd, wr
end

--- splice(2) from one fd into another.
function M.splice(vm, fd_in, fd_out, length, flags)
    return vm:syscall(M.NR.splice, fd_in, 0, fd_out, 0, length, flags or 0)
end

--- copy_file_range(2).
function M.copy_file_range(vm, fd_in, fd_out, length)
    return vm:syscall(M.NR.copy_file_range, fd_in, 0, fd_out, 0, length, 0)
end

--- mmap(2). Returns the address, or `nil, errno`.
function M.mmap(vm, fd, length, prot, flags, offset)
    local r = vm:syscall(M.NR.mmap, 0, length, prot, flags, fd, offset or 0)
    -- mmap reports failure as a small negative value in the return
    -- register; anything at or above -4095 is an errno.
    if r.ret < 0 then return nil, r.errno end
    return r.ret
end

--- munmap(2).
function M.munmap(vm, addr, length)
    return vm:syscall(M.NR.munmap, addr, length)
end

--- mknod(2), as mknodat(AT_FDCWD). `mode` carries the type bits.
function M.mknod(vm, path, mode, dev)
    return vm:syscall(M.NR.mknodat, {
        args = { M.AT_FDCWD, 0, mode, dev or 0 },
        bufs = { M.cstr(path) },
        ptrs = { 1 },
    })
end

--- unlinkat(2). Pass `M.AT_REMOVEDIR` to remove a directory.
function M.unlink(vm, path, flags)
    return vm:syscall(M.NR.unlinkat, {
        args = { M.AT_FDCWD, 0, flags or 0 },
        bufs = { M.cstr(path) },
        ptrs = { 1 },
    })
end

--- renameat2(2), without asserting.
function M.rename(vm, from, to, flags)
    return vm:syscall(M.NR.renameat2, {
        args = { M.AT_FDCWD, 0, M.AT_FDCWD, 0, flags or 0 },
        bufs = { M.cstr(from), M.cstr(to) },
        ptrs = { 1, 3 },
    })
end

--- link(2), as linkat(AT_FDCWD).
function M.link(vm, from, to, flags)
    return vm:syscall(M.NR.linkat, {
        args = { M.AT_FDCWD, 0, M.AT_FDCWD, 0, flags or 0 },
        bufs = { M.cstr(from), M.cstr(to) },
        ptrs = { 1, 3 },
    })
end

--- lseek(2).
function M.lseek(vm, fd, offset, whence)
    return vm:syscall(M.NR.lseek, fd, offset, whence or 0)
end

--- readlink(2), as readlinkat(AT_FDCWD). Returns the target, or
--- `nil, errno`.
function M.readlink(vm, path, size)
    size = size or 4096
    local r = vm:syscall(M.NR.readlinkat, {
        args = { M.AT_FDCWD, 0, 0, size },
        bufs = { M.cstr(path), string.rep("\0", size) },
        ptrs = { 1, 2 },
    })
    if r.ret < 0 then return nil, r.errno end
    return r.out_bufs[2]:sub(1, r.ret)
end

--- chmod(2), as fchmodat(AT_FDCWD).
function M.chmod(vm, path, mode)
    return vm:syscall(M.NR.fchmodat, {
        args = { M.AT_FDCWD, 0, mode, 0 },
        bufs = { M.cstr(path) },
        ptrs = { 1 },
    })
end

--- chown(2), as fchownat(AT_FDCWD). `-1` leaves a field alone.
---
--- `opts.follow = false` uses AT_SYMLINK_NOFOLLOW, which is what a
--- case about a symlink itself needs — the default follows, and a
--- dangling link then fails ENOENT rather than touching the link.
function M.chown(vm, path, uid, gid, opts)
    local flags = (opts and opts.follow == false) and M.AT_SYMLINK_NOFOLLOW or 0
    return vm:syscall(M.NR.fchownat, {
        args = { M.AT_FDCWD, 0, uid or -1, gid or -1, flags },
        bufs = { M.cstr(path) },
        ptrs = { 1 },
    })
end

--- openat(2) at AT_FDCWD. Returns the fd, or `nil, errno`.
---
--- The agent issues every syscall from one process, so an fd stays
--- open across calls and can be handed to `M.ioctl` or `M.close`.
function M.open(vm, path, flags, mode)
    return M.openat(vm, M.AT_FDCWD, path, flags, mode)
end

--- openat(2) relative to an open directory fd.
---
--- The settled-participant-set cases need this: resolving a name
--- *through* a directory descriptor is an ordinary live resolution,
--- and must see strata the descriptor's own enumeration does not.
function M.openat(vm, dirfd, path, flags, mode)
    local r = vm:syscall(M.NR.openat, {
        args = { dirfd, 0, flags or M.O.RDONLY, mode or 0 },
        bufs = { M.cstr(path) },
        ptrs = { 1 },
    })
    if r.ret < 0 then return nil, r.errno end
    return r.ret
end

--- write(2). Returns the raw syscall result.
function M.write(vm, fd, data)
    return vm:syscall(M.NR.write, {
        args = { fd, 0, #data },
        bufs = { data },
        ptrs = { 1 },
    })
end

--- nanosleep(2), for polling loops that must yield rather than spin.
function M.nanosleep(vm, sec, nsec)
    return vm:syscall(M.NR.nanosleep, {
        args = { 0, 0 },
        bufs = { string.pack("<i8i8", sec, nsec) },
        ptrs = { 0 },
    })
end

--- read(2). Returns the data (possibly short, "" at EOF), or `nil, errno`.
function M.read(vm, fd, size)
    local r = vm:syscall(M.NR.read, {
        args = { fd, 0, size },
        bufs = { string.rep("\0", size) },
        ptrs = { 1 },
    })
    if r.ret < 0 then return nil, r.errno end
    return r.out_bufs[1]:sub(1, r.ret)
end

--- close(2).
function M.close(vm, fd) return vm:syscall(M.NR.close, fd) end

--- ioctl(2) with a pointer to an in/out word — the shape the inode
--- flag ioctls take. Returns the word back, or `nil, errno`.
function M.ioctl_word(vm, fd, cmd, word)
    local r = vm:syscall(M.NR.ioctl, {
        args = { fd, cmd, 0 },
        bufs = { string.pack("<I8", word or 0) },
        ptrs = { 2 },
    })
    if r.ret ~= 0 then return nil, r.errno end
    return string.unpack("<I8", r.out_bufs[1])
end

--- Set or clear FS_IMMUTABLE_FL on a path.
---
--- The third term of the accepts-modification predicate (§4.2.1) is
--- specifically the immutable inode flag, so a test that wants a
--- provider which refuses modification *without* saying `ro` and
--- *without* a read-only mount sets this.
function M.set_immutable(vm, path, on)
    local fd, errno = M.open(vm, path, M.O.RDONLY)
    if not fd then return nil, errno end
    local flags, err = M.ioctl_word(vm, fd, M.FS_IOC_GETFLAGS, 0)
    if flags then
        if on == false then
            flags = flags & ~M.FS_IMMUTABLE_FL
        else
            flags = flags | M.FS_IMMUTABLE_FL
        end
        flags, err = M.ioctl_word(vm, fd, M.FS_IOC_SETFLAGS, flags)
    end
    M.close(vm, fd)
    if not flags then return nil, err end
    return true
end

--- flock(2).
function M.flock(vm, fd, operation)
    return vm:syscall(M.NR.flock, fd, operation)
end

--- fsync(2).
function M.fsync(vm, fd) return vm:syscall(M.NR.fsync, fd) end

--- mkdir(2), without asserting.
function M.mkdir(vm, path, mode)
    return vm:syscall(M.NR.mkdir, {
        args = { 0, mode or tonumber("755", 8) },
        bufs = { M.cstr(path) },
        ptrs = { 0 },
    })
end

--- mkdir(2) including any missing parents, without asserting.
---
--- `vm:mkdir` does this already; this is the version a worker can use,
--- since a worker exposes `syscall` and little else.
function M.mkdir_p(who, path)
    local made = { }
    local at = ""
    for part in path:gmatch("[^/]+") do
        at = at .. "/" .. part
        local r = M.mkdir(who, at)
        if r.ret ~= 0 and r.errno ~= M.E.EXIST then return r end
        made[#made + 1] = at
    end
    return { ret = 0, errno = 0 }
end

--- A worker process in a new user namespace and mount namespace.
---
--- Everything the agent does otherwise runs as root in the initial
--- namespaces, which is the one thing §4.2.3's entitlement cases need
--- not to be. The worker is a separate process, so its namespaces and
--- its mounts are its own and nothing here leaks back.
---
--- With no `uid_map` written the caller's uid maps to the overflow uid,
--- which is what makes it unprivileged outside the new namespace while
--- holding a full capability set inside it. Returns `nil, errno` where
--- the unshare is refused.
---
--- Every `M.*` call in this module takes the worker in place of the vm:
--- both expose the same `syscall` method.
function M.unprivileged_worker(vm)
    local worker = vm:spawn_worker()
    local r = worker:syscall(M.NR.unshare, M.CLONE_NEWUSER | M.CLONE_NEWNS)
    if r.ret ~= 0 then return nil, r.errno end
    return worker
end

--- Bind-mount `from` at `to`, read-only.
---
--- Two calls: the kernel ignores MS_RDONLY on the bind itself, so the
--- read-only-ness has to be applied by a second remount. This is how a
--- test produces a provider whose *mount* is read-only, as distinct
--- from a stratum flagged `ro`.
function M.bind_ro(vm, from, to)
    local r = M.mount(vm, { source = from, target = to, flags = M.MS_BIND })
    if r.ret ~= 0 then return r end
    return M.mount(vm, {
        target = to,
        flags = M.MS_BIND | M.MS_REMOUNT | M.MS_RDONLY,
    })
end

return M
