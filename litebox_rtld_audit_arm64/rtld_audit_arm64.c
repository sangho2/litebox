// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

// ARM64 version of the rtld_audit library for dynamic library interception.
// This library is loaded via LD_AUDIT and intercepts shared library loading
// to patch trampolines in rewritten binaries.

#define _GNU_SOURCE
#include <assert.h>
#include <elf.h>
#include <link.h>
#include <stdint.h>

#define TARGET_SECTION_NAME ".trampolineLB0"
#define HEADER_MAGIC ((uint32_t)0x5842544c) // "LTBX"
#define TRAMP_MAGIC ((uint64_t)0x30584f424554494c)  // "LITEBOX0"

#if !defined(__aarch64__)
# error "rtld_audit_arm64.c: build target must be aarch64"
#endif

// Linux syscall numbers (arm64)
#define SYS_openat 56
#define SYS_close 57
#define SYS_read 63
#define SYS_write 64
#define SYS_fstat 80
#define SYS_exit_group 94
#define SYS_mmap 222
#define SYS_mprotect 226
#define SYS_munmap 215
#define AT_FDCWD -100

// Linux flags
#define MAP_PRIVATE 0x02
#define MAP_FIXED 0x10

#define PROT_READ 0x1
#define PROT_WRITE 0x2
#define PROT_EXEC 0x4

typedef long (*syscall_stub_t)(void);
static syscall_stub_t syscall_entry = 0;
// Address of the trampoline data section where host TLS is stored at offset 16
static void *trampoline_data = 0;
static char interp[256] = {0}; // Buffer for interpreter path

#ifdef DEBUG
#define syscall_print(str, len)                                                \
  do_syscall(SYS_write, 1, (long)(str), len, 0, 0, 0)
#else
#define syscall_print(str, len)
#endif

// Direct syscall without going through trampoline - for early debug output
// before syscall_entry is set
__attribute__((unused))
static long direct_syscall(long num, long a0, long a1, long a2, long a3, long a4,
                           long a5) {
  register long x8 __asm__("x8") = num;
  register long x0 __asm__("x0") = a0;
  register long x1 __asm__("x1") = a1;
  register long x2 __asm__("x2") = a2;
  register long x3 __asm__("x3") = a3;
  register long x4 __asm__("x4") = a4;
  register long x5 __asm__("x5") = a5;

  __asm__ volatile(
      "svc #0\n"
      : "+r"(x0)
      : "r"(x8), "r"(x1), "r"(x2), "r"(x3), "r"(x4), "r"(x5)
      : "memory");
  return x0;
}

// Early debug print that works before syscall_entry is set
// This uses direct syscalls which will be intercepted by the shim
#ifdef DEBUG
#define early_print(str, len) direct_syscall(SYS_write, 1, (long)(str), len, 0, 0, 0)
#else
#define early_print(str, len)
#endif

static void early_print_hex(uint64_t data) {
#ifdef DEBUG
  for (int i = 15; i >= 0; i--) {
    unsigned char byte = (data >> (i * 4)) & 0xF;
    if (byte < 10) {
      early_print((&"0123456789"[byte]), 1);
    } else {
      early_print((&"abcdef"[byte - 10]), 1);
    }
  }
  early_print("\n", 1);
#endif
}

// ARM64 syscall: arguments in x0-x5, syscall number in x8, return in x0
// The syscall_callback expects:
// - x18 = host TLS (loaded from trampoline_data+16)
// - Stack frame set up like the trampoline does:
//   [SP+0]: saved x16, [SP+8]: saved x17, [SP+16]: saved x30
// - x30 (LR) = return address after syscall
static long do_syscall(long num, long a0, long a1, long a2, long a3, long a4,
                       long a5) {
  if (!syscall_entry || !trampoline_data)
    return -1;

  // Load host TLS from trampoline_data+16
  uint64_t host_tls;
  __builtin_memcpy(&host_tls, (char *)trampoline_data + 16, sizeof(host_tls));

  register long x8 __asm__("x8") = num;
  register long x0 __asm__("x0") = a0;
  register long x1 __asm__("x1") = a1;
  register long x2 __asm__("x2") = a2;
  register long x3 __asm__("x3") = a3;
  register long x4 __asm__("x4") = a4;
  register long x5 __asm__("x5") = a5;
  register uint64_t x18 __asm__("x18") = host_tls;

  // Set up the stack frame like the trampoline does:
  // [SP+0]: x16, [SP+8]: x17, [SP+16]: x30 (return address)
  // Then call syscall_callback. After it processes the syscall,
  // it will call switch_to_guest which:
  // 1. Restores SP to the original guest SP (current SP + 32)
  // 2. Jumps to the saved return address
  // So we DON'T need to restore SP manually - switch_to_guest does it.
  __asm__ volatile(
      // Set up stack frame like trampoline
      "sub sp, sp, #32\n"
      "str x16, [sp, #0]\n"
      "str x17, [sp, #8]\n"
      // Save return address at [SP+16]
      "adr x16, 1f\n"           // x16 = address of label 1 (return point)
      "str x16, [sp, #16]\n"    // Save return address at [SP+16]
      "blr %[entry]\n"          // Call syscall_callback
      // switch_to_guest will jump back here after processing the syscall.
      // At this point, SP has already been restored to original_sp by switch_to_guest.
      // All registers have been restored from ctx (x0 contains syscall return value).
      "1:\n"
      // No stack cleanup needed - switch_to_guest already restored SP
      : "+r"(x0)
      : [entry] "r"(syscall_entry), "r"(x8), "r"(x1), "r"(x2), "r"(x3),
        "r"(x4), "r"(x5), "r"(x18)
      : "x16", "x17", "x30", "memory");
  return x0;
}

/* Re-implement some utility functions to avoid dependency on libc. */

// Define the FileStat structure for ARM64
struct FileStat {
  unsigned long st_dev;
  unsigned long st_ino;
  unsigned int st_mode;
  unsigned int st_nlink;
  unsigned int st_uid;
  unsigned int st_gid;
  unsigned long st_rdev;
  unsigned long __pad1;
  long st_size;
  int st_blksize;
  int __pad2;
  long st_blocks;
  long st_atime;
  unsigned long st_atime_nsec;
  long st_mtime;
  unsigned long st_mtime_nsec;
  long st_ctime;
  unsigned long st_ctime_nsec;
  unsigned int __unused4;
  unsigned int __unused5;
};

int memcmp(const void *s1, const void *s2, size_t n) {
  const unsigned char *p1 = s1;
  const unsigned char *p2 = s2;
  while (n--) {
    if (*p1 != *p2) {
      return *p1 - *p2;
    }
    p1++;
    p2++;
  }
  return 0;
}

int strcmp(const char *s1, const char *s2) {
  while (*s1 && (*s1 == *s2)) {
    s1++;
    s2++;
  }
  return *(unsigned char *)s1 - *(unsigned char *)s2;
}

char *strncpy(char *dest, const char *src, size_t n) {
  char *d = dest;
  const char *s = src;
  while (n-- && *s) {
    *d++ = *s++;
  }
  while (n--) {
    *d++ = '\0';
  }
  return dest;
}

static uint64_t read_u64(const void *p) {
  uint64_t v;
  __builtin_memcpy(&v, p, 8);
  return v;
}

static size_t align_up(size_t val, size_t align) {
  return (val + align - 1) & ~(align - 1);
}

unsigned int la_version(unsigned int version __attribute__((unused))) {
  return LAV_CURRENT;
}

/// print value in hex
void print_hex(uint64_t data) {
#ifdef DEBUG
  for (int i = 15; i >= 0; i--) {
    unsigned char byte = (data >> (i * 4)) & 0xF;
    if (byte < 10) {
      syscall_print((&"0123456789"[byte]), 1);
    } else {
      syscall_print((&"abcdef"[byte - 10]), 1);
    }
  }
  syscall_print("\n", 1);
#endif
}

/// @brief Parse object to find the syscall entry point and the interpreter
/// path.
///
/// For ARM64, the trampoline layout is:
/// - Data section at trampoline_addr (1 page = 0x1000 bytes):
///   - offset 0: magic (8 bytes) = TRAMP_MAGIC
///   - offset 8: handler address (8 bytes) - we write the syscall_entry here
///   - offset 16: host TLS pointer (8 bytes) - written by switch_to_guest
/// - Code section at trampoline_addr + 0x1000:
///   - Per-SVC trampoline entries
///
/// The trampoline vaddr is placed at: page_align_up(max_vaddr) + 0x400000
/// where max_vaddr is the end of the last PT_LOAD segment.
/// This 4MB offset matches the rewriter's find_addr_for_trampoline_code().
int parse_object(const struct link_map *map) {
  early_print("[audit-arm64] parse_object l_addr=", 34);
  early_print_hex((uint64_t)map->l_addr);
  
  unsigned long max_addr = 0;
  Elf64_Ehdr *eh = (Elf64_Ehdr *)map->l_addr;
  if (memcmp(eh->e_ident,
             "\x7f"
             "ELF",
             4) != 0) {
    early_print("[audit-arm64] not an ELF file\n", 30);
    return 1;
  }
  
  early_print("[audit-arm64] valid ELF, e_phnum=", 33);
  early_print_hex(eh->e_phnum);
  
  Elf64_Phdr *phdrs = (Elf64_Phdr *)((char *)map->l_addr + eh->e_phoff);
  for (int i = 0; i < eh->e_phnum; i++) {
    if (phdrs[i].p_type == PT_LOAD) {
      unsigned long vaddr_end = (phdrs[i].p_vaddr + phdrs[i].p_memsz);
      if (vaddr_end > max_addr) {
        max_addr = vaddr_end;
      }
    } else if (phdrs[i].p_type == PT_INTERP) {
      strncpy(interp, (char *)map->l_addr + phdrs[i].p_vaddr,
              sizeof(interp) - 1);
      interp[sizeof(interp) - 1] = '\0';
    }
  }
  
  // Calculate trampoline vaddr using the same formula as the rewriter:
  // trampoline_vaddr = page_align_up(max_vaddr) + 0x400000 (4MB offset)
  max_addr = align_up(max_addr, 0x1000);
  uint64_t trampoline_vaddr = max_addr + 0x400000;
  void *data_section_addr = (void *)(map->l_addr + trampoline_vaddr);
  
  early_print("[audit-arm64] max_addr=", 23);
  early_print_hex(max_addr);
  early_print("[audit-arm64] trampoline_vaddr=", 31);
  early_print_hex(trampoline_vaddr);
  early_print("[audit-arm64] data_section_addr=", 32);
  early_print_hex((uint64_t)data_section_addr);
  
  uint64_t magic = read_u64(data_section_addr);
  early_print("[audit-arm64] read magic=", 25);
  early_print_hex(magic);
  
  if (magic != TRAMP_MAGIC) {
    early_print("[audit-arm64] invalid trampoline magic!\n", 40);
    early_print("[audit-arm64] expected=", 23);
    early_print_hex(TRAMP_MAGIC);
    return 1;
  }
  
  // Handler address is at offset 8 in the data section
  syscall_entry = (syscall_stub_t)read_u64(data_section_addr + 8);
  // Store trampoline data address for do_syscall to load host TLS from offset 16
  trampoline_data = data_section_addr;
  early_print("[audit-arm64] got syscall entry: ", 33);
  early_print_hex((uint64_t)syscall_entry);
  return 0;
}

unsigned int la_objopen(struct link_map *map,
                        Lmid_t lmid __attribute__((unused)),
                        uintptr_t *cookie __attribute__((unused))) {
  early_print("[audit-arm64] la_objopen called\n", 32);
  const char *path = map->l_name;

  if (!path || path[0] == '\0') {
    early_print("[audit-arm64] main binary, l_addr=", 34);
    early_print_hex((uint64_t)map->l_addr);
    // main binary should be called first.
    // For dynamically linked binaries, the main binary typically has no
    // syscalls (all syscalls are in libc). We'll get syscall_entry from
    // ld-linux.so instead. Trying to parse the main binary would crash
    // because the trampoline memory isn't mapped.
    //
    // TODO: If we need to support main binaries with syscalls, we would need
    // to check if the .trampolineLB0 section exists first, which requires
    // reading the file from disk.
    if (map->l_addr != 0) {
      // Extract interpreter path from PT_INTERP for later comparison
      Elf64_Ehdr *eh = (Elf64_Ehdr *)map->l_addr;
      if (memcmp(eh->e_ident, "\x7f" "ELF", 4) == 0) {
        Elf64_Phdr *phdrs = (Elf64_Phdr *)((char *)map->l_addr + eh->e_phoff);
        for (int i = 0; i < eh->e_phnum; i++) {
          if (phdrs[i].p_type == PT_INTERP) {
            strncpy(interp, (char *)map->l_addr + phdrs[i].p_vaddr,
                    sizeof(interp) - 1);
            interp[sizeof(interp) - 1] = '\0';
            early_print("[audit-arm64] interp=", 21);
            for (int j = 0; j < 60 && interp[j]; j++) {
              early_print(&interp[j], 1);
            }
            early_print("\n", 1);
            break;
          }
        }
      }
      early_print("[audit-arm64] skipping main binary (no trampoline check)\n", 57);
    } else {
      early_print("[audit-arm64] main binary l_addr=0 (non-PIE)\n", 45);
    }
    return 0; // main binary handled
  }

  early_print("[audit-arm64] lib path: ", 24);
  // Print first 60 chars of path
  for (int i = 0; i < 60 && path[i]; i++) {
    early_print(&path[i], 1);
  }
  early_print("\n", 1);

  if (syscall_entry == 0) {
    early_print("[audit-arm64] syscall_entry=0, trying ld.so\n", 44);
    // failed to get the syscall entry point from the main binary
    // fall back to get it from ld-*.so, which should be called next.
    if (parse_object(map) != 0) {
      early_print("[audit-arm64] ld.so also has no trampoline!\n", 44);
      return 0;
    }
    early_print("[audit-arm64] got syscall entry from ld.so\n", 43);
    return 0; // ld.so is patched by libOS
  }

  if (interp[0] != '\0' && strcmp(path, interp) == 0) {
    // successfully get the entry point and interpreter from the main binary
    syscall_print("[audit-arm64] ld-*.so is patched by libOS\n", 42);
    return 0; // ld.so is patched by libOS
  }

  // Other shared libraries
  syscall_print("[audit-arm64] la_objopen: path=", 31);
  syscall_print(path, 32);
  syscall_print("\n", 1);
  syscall_print("[audit-arm64] lib l_addr=", 25);
  print_hex(map->l_addr);

  if (!syscall_entry) {
    return 0;
  }

  int fd = do_syscall(SYS_openat, AT_FDCWD, (long)path, 0, 0, 0, 0);
  if (fd < 0) {
    syscall_print("[audit-arm64] failed to open file\n", 34);
    return 0;
  }

  struct FileStat st;
  if (do_syscall(SYS_fstat, fd, (long)&st, 0, 0, 0, 0) < 0) {
    syscall_print("[audit-arm64] fstat failed\n", 27);
    do_syscall(SYS_close, fd, 0, 0, 0, 0, 0);
    return 0;
  }
  long file_size = st.st_size;

  void *map_base =
      (void *)do_syscall(SYS_mmap, 0, file_size, PROT_READ, MAP_PRIVATE, fd, 0);
  if ((uintptr_t)map_base >= (uintptr_t)-4096) {
    syscall_print("[audit-arm64] mmap failed\n", 26);
    do_syscall(SYS_close, fd, 0, 0, 0, 0, 0);
    return 0;
  }

  Elf64_Ehdr *eh = (Elf64_Ehdr *)map_base;
  if (memcmp(eh->e_ident,
             "\x7f"
             "ELF",
             4) != 0) {
    syscall_print("[audit-arm64] not an ELF file\n", 30);
    do_syscall(SYS_close, fd, 0, 0, 0, 0, 0);
    return 0;
  }

  Elf64_Shdr *shdrs = (Elf64_Shdr *)((char *)map_base + eh->e_shoff);
  Elf64_Shdr *shstr = &shdrs[eh->e_shstrndx];
  const char *shnames = (char *)map_base + shstr->sh_offset;

  for (int i = 0; i < eh->e_shnum; i++) {
    const char *name = shnames + shdrs[i].sh_name;
    if (strcmp(name, TARGET_SECTION_NAME) != 0)
      continue;

    syscall_print("[audit-arm64] found trampoline section\n", 39);
    // Note sh_addr, sh_offset, and sh_entsize are repurposed to store our trampoline info.
    // See litebox_syscall_rewriter_arm64 for details.
    if (shdrs[i].sh_addr != HEADER_MAGIC) {
      syscall_print("[audit-arm64] invalid header magic\n", 35);
      break;
    }

    // sh_offset contains the virtual address where data section should be mapped
    // sh_entsize contains the total size (data section + code section)
    uint64_t data_section_vaddr = map->l_addr + shdrs[i].sh_offset;
    uint64_t total_size = shdrs[i].sh_entsize;
    uint64_t tramp_file_offset = file_size - total_size;
    
    syscall_print("[audit-arm64] libc tramp vaddr=", 31);
    print_hex(data_section_vaddr);
    syscall_print("[audit-arm64] libc tramp size=", 30);
    print_hex(total_size);
    
    // Data section is 0x1000 bytes (one page)
    uint64_t data_size = 0x1000;
    // Code section follows data section
    uint64_t code_vaddr = data_section_vaddr + data_size;
    uint64_t code_size = total_size - data_size;
    uint64_t code_size_aligned = align_up(code_size, 0x1000);

    // Map data section as RW
    void *data_mapped =
        (void *)do_syscall(SYS_mmap, data_section_vaddr, data_size,
                           PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_FIXED, fd, tramp_file_offset);
    if ((uintptr_t)data_mapped >= (uintptr_t)-4096) {
      syscall_print("[audit-arm64] mmap failed for data section\n", 43);
      break;
    }
    if ((uint64_t)data_mapped != data_section_vaddr) {
      syscall_print("[audit-arm64] data mmap returned unexpected address\n", 51);
      print_hex((uint64_t)data_mapped);
      do_syscall(SYS_munmap, (long)data_mapped, data_size, 0, 0, 0, 0);
      break;
    }

    // Validate magic
    if (read_u64(data_mapped) != TRAMP_MAGIC) {
      syscall_print("[audit-arm64] invalid trampoline magic in data\n", 47);
      break;
    }

    // Write the syscall entry point to offset 8 in data section
    __builtin_memcpy((char *)data_mapped + 8, (const void *)&syscall_entry, 8);
    syscall_print("[audit-arm64] patched handler address\n", 38);

    // Copy host TLS from ld-linux's trampoline to this library's trampoline
    // Host TLS is stored at offset 16 in the trampoline data section
    if (trampoline_data != 0) {
      uint64_t host_tls;
      __builtin_memcpy(&host_tls, (char *)trampoline_data + 16, sizeof(host_tls));
      syscall_print("[audit-arm64] host_tls value=", 29);
      print_hex(host_tls);
      __builtin_memcpy((char *)data_mapped + 16, (const void *)&host_tls, 8);
      syscall_print("[audit-arm64] copied host TLS\n", 30);
    }

    // Map code section as R-X if there is any
    if (code_size > 0) {
      void *code_mapped =
          (void *)do_syscall(SYS_mmap, code_vaddr, code_size_aligned,
                             PROT_READ | PROT_EXEC, MAP_PRIVATE | MAP_FIXED, fd, 
                             tramp_file_offset + data_size);
      if ((uintptr_t)code_mapped >= (uintptr_t)-4096) {
        syscall_print("[audit-arm64] mmap failed for code section\n", 43);
        break;
      }
      if ((uint64_t)code_mapped != code_vaddr) {
        syscall_print("[audit-arm64] code mmap returned unexpected address\n", 51);
        print_hex((uint64_t)code_mapped);
        do_syscall(SYS_munmap, (long)code_mapped, code_size_aligned, 0, 0, 0, 0);
        break;
      }
    }

    syscall_print("[audit-arm64] trampoline loaded successfully\n", 45);
    break;
  }

  syscall_print("[audit-arm64] closing fd\n", 25);
  do_syscall(SYS_close, fd, 0, 0, 0, 0, 0);
  syscall_print("[audit-arm64] unmapping\n", 24);
  do_syscall(SYS_munmap, (long)map_base, file_size, 0, 0, 0, 0);
  syscall_print("[audit-arm64] returning\n", 24);
  return 0;
}
