/*
 * NKDSBP2.dll proxy — logs NkDriverEntry calls AND SCSI CDBs.
 *
 * NkDriverEntry: __stdcall, 3 params (12 bytes), confirmed from RET 0xc.
 *   int NkDriverEntry(DWORD op, DWORD param2, DWORD param3)
 *
 * SCSI capture: IAT-hooks DeviceIoControl in Nkdsbp2_real.dll ONLY (not the
 * whole process), so only calls originating from the real Nikon transport DLL
 * are intercepted. This avoids the global inline-patch problems seen earlier.
 *
 * For IOCTL_SCSISCAN_CMD (0x190012) the SCSISCAN_CMD control struct carries:
 *   - Cdb[16]        : the SCSI command descriptor block
 *   - Direction      : 0=none, 0x40=IN(device->host), 0x80=OUT(host->device)
 *   - TransferLength : byte count of the data buffer
 *
 * Data-OUT (0x80, e.g. SET WINDOW) bytes are logged BEFORE the call.
 * Data-IN  (0x40, e.g. GET WINDOW / READ) bytes are logged AFTER the call.
 *
 * Overlapped I/O handling:
 *   The Nikon driver issues ALL SCSI commands via overlapped (async) I/O —
 *   DeviceIoControl returns FALSE with ERROR_IO_PENDING. Small transfers
 *   (INQUIRY, GET WINDOW, etc.) complete fast enough that the buffer is
 *   filled by the time our hook reads it. Large transfers (image reads,
 *   376KB-804KB) do NOT complete in time — the DMA is still in flight when
 *   we try to read lpOutBuffer. The fix: after calling realDeviceIoControl,
 *   if the call returned FALSE (pending) and lpOverlapped is provided, we
 *   POLL HasOverlappedIoCompleted(lpOverlapped) (reads OVERLAPPED.Internal,
 *   set directly by the OS) until it flips, instead of waiting on the
 *   completion event ourselves. Waiting on the event would consume it
 *   (auto-reset) or force us to manually re-signal it for the caller —
 *   either way we'd be perturbing a synchronization object the caller also
 *   depends on. Polling never touches it, so the caller's own subsequent
 *   wait on that event behaves exactly as it would against the real DLL.
 *   We also save/restore GetLastError() around all of this — WriteFile,
 *   Sleep, etc. all clobber it as a side effect, and the caller's overlapped
 *   pattern depends on seeing ERROR_IO_PENDING survive on a pending call.
 *   (An earlier version of this fix used WaitForSingleObject + SetEvent and
 *   didn't preserve last-error — it broke device detection entirely: every
 *   SCSI command succeeded, but Nikon Scan saw a clobbered error code after
 *   each one and gave up, reporting "device not found".)
 *
 * Build:
 *   i686-w64-mingw32-gcc -shared -O2 -o NKDSBP2.dll proxy.c \
 *       -lkernel32 -Wl,--enable-stdcall-fixup nkdsbp2.def
 */

#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <stdint.h>
#include <string.h>

#ifndef PROXY_REAL_DLL
#define PROXY_REAL_DLL "Nkdsbp2_real.dll"
#endif

#ifndef PROXY_LABEL
#define PROXY_LABEL "NKDSBP2.dll"
#endif

#ifndef PROXY_TRACE_PREFIX
#define PROXY_TRACE_PREFIX "nkdsbp2"
#endif

/* The LS-8000 verification proxy records commands without delaying Nikon's
 * asynchronous reads or inspecting MAID buffers. The older LS-5000 research
 * build can opt into those invasive captures explicitly. */
#ifndef PROXY_WAIT_FOR_DATA_IN
#define PROXY_WAIT_FOR_DATA_IN 1
#endif
#ifndef PROXY_TRANSPORT_ONLY
#define PROXY_TRANSPORT_ONLY 0
#endif

/* GetProcAddress returns FARPROC; casting to a specific function pointer type
 * is the documented usage but trips -Wcast-function-type. */
#pragma GCC diagnostic ignored "-Wcast-function-type"

/* IOCTL_SCSISCAN_CMD = FILE_DEVICE_SCANNER(0x22) << 16 | METHOD_OUT_DIRECT(2) | function(4) << 2 */
#define IOCTL_SCSISCAN_CMD 0x00190012

/* SCSISCAN_CMD struct layout — from Microsoft's scsiscan.h (Windows DDK).
 * This is the PUBLIC spec, not reverse-engineered. */
#pragma pack(push, 4)
typedef struct {
    ULONG  Reserved1;       /* +0x00                                   */
    ULONG  Size;            /* +0x04: sizeof(SCSISCAN_CMD) = 0x2C      */
    ULONG  SrbFlags;        /* +0x08: SRB_FLAGS_DATA_IN=0x40, _OUT=0x80 */
    UCHAR  CdbLength;       /* +0x0C: 6, 10, or 16                     */
    UCHAR  SenseLength;     /* +0x0D: sense buffer size                */
    UCHAR  Reserved2;       /* +0x0E                                   */
    UCHAR  Reserved3;       /* +0x0F                                   */
    ULONG  TransferLength;  /* +0x10: byte count of data buffer        */
    UCHAR  Cdb[16];         /* +0x14: SCSI CDB                         */
    PUCHAR pSrbStatus;      /* +0x24: ptr to SRB status byte           */
    PUCHAR pSenseBuffer;    /* +0x28: ptr to sense data buffer         */
} SCSISCAN_CMD;
#pragma pack(pop)

/* SRB flag bits (from scsiscan.h) */
#define SRB_FLAGS_DATA_IN    0x00000040
#define SRB_FLAGS_DATA_OUT  0x00000080

/* --- Globals --- */
static HANDLE g_logFile = INVALID_HANDLE_VALUE;  /* human-readable text trace */
static HANDLE g_binFile = INVALID_HANDLE_VALUE;  /* full binary trace (untruncated) */
static HINSTANCE g_selfDll = NULL;
static HMODULE g_realDll = NULL;
/* 0 not started, 1 in progress, 2 ready, -1 failed. The real DLL is loaded
 * on the first exported call, outside DllMain and the Windows loader lock. */
static volatile LONG g_initState = 0;
static DWORD g_seq = 0;  /* monotonic call index for log correlation */
static char g_captureDir[MAX_PATH] = "C:\\";
static BOOL g_builderHooked = FALSE;
static BOOL g_builderAttempted = FALSE;
static LONG g_builderSeq = 0;

/* Scanner-module addresses, stored as RVAs so relocation does not matter.
 * Each target is verified against the matching Nikon Scan 4.0.3 module before
 * its capture package is built. The call-site checks in try_hook_builder keep
 * a different module revision from being patched at an unknown address. */
#ifdef PROXY_BUILDER_LS8000
#define BUILDER_MODULE_NAME "LS8000.md3"
#define BUILDER_TARGET_NAME "LS-8000"
#define NIKON_BUILDER_RVA 0x00012690
#else
#define BUILDER_MODULE_NAME "LS5000.md3"
#define BUILDER_TARGET_NAME "LS-5000"
#define NIKON_BUILDER_RVA 0x00011840
#endif
#define BUILDER_DESCRIPTOR_SIZE 0x84
#ifdef PROXY_BUILDER_LS8000
static const DWORD g_builderCallRvas[] = {
    0x0003E552, 0x0003EEEB, 0x0008665B,
    0x00086973, 0x00086EB0, 0x0008B4FF
};
#else
static const DWORD g_builderCallRvas[] = {
    0x00040AA5, 0x00041944, 0x0008A965, 0x0008AC71
};
#endif

typedef int (__cdecl *pNikonBuilder)(void *descriptor);
static pNikonBuilder g_realNikonBuilder = NULL;

/* Real DeviceIoControl pointer (resolved via GetProcAddress; IAT slot is
 * overwritten to point at our hook). */
typedef BOOL (WINAPI *pDeviceIoControl)(
    HANDLE hDevice, DWORD dwIoControlCode,
    LPVOID lpInBuffer, DWORD nInBufferSize,
    LPVOID lpOutBuffer, DWORD nOutBufferSize,
    LPDWORD lpBytesReturned, LPOVERLAPPED lpOverlapped);
static pDeviceIoControl realDeviceIoControl = NULL;

/* 3 params, __stdcall — matches RET 0xc in the real DLL */
typedef int (WINAPI *pNkDriverEntry)(DWORD op, DWORD param2, DWORD param3);
static pNkDriverEntry g_realNkDriverEntry = NULL;

/* --- Logging helpers --- */

static void log_write(const char *fmt, ...) {
    if (g_logFile == INVALID_HANDLE_VALUE) return;
    char buf[4096];
    va_list args;
    va_start(args, fmt);
    int len = wvsprintfA(buf, fmt, args);
    va_end(args);
    DWORD written;
    WriteFile(g_logFile, buf, len, &written, NULL);
    FlushFileBuffers(g_logFile);
}

static void log_hex(const char *prefix, const BYTE *data, int len) {
    if (g_logFile == INVALID_HANDLE_VALUE) return;
    if (data == NULL || len <= 0) return;
    if (IsBadReadPtr(data, len)) return;  /* avoid crashes on bad pointers */
    char line[512];
    int pos = 0;
    pos += wsprintfA(line + pos, "%s (%d bytes):", prefix, len);
    for (int i = 0; i < len && pos < 500; i++) {
        pos += wsprintfA(line + pos, " %02X", data[i]);
    }
    pos += wsprintfA(line + pos, "\r\n");
    DWORD written;
    WriteFile(g_logFile, line, pos, &written, NULL);
    FlushFileBuffers(g_logFile);
}

static void capture_path(char *out, const char *name) {
    lstrcpynA(out, g_captureDir, MAX_PATH);
    size_t len = strlen(out);
    if (len > 0 && out[len - 1] != '\\' && out[len - 1] != '/') {
        lstrcatA(out, "\\");
    }
    lstrcatA(out, name);
}

static HANDLE open_capture(const char *name, DWORD disposition) {
    char path[MAX_PATH];
    capture_path(path, name);
    return CreateFileA(path, GENERIC_WRITE, FILE_SHARE_READ, NULL,
                       disposition, FILE_ATTRIBUTE_NORMAL, NULL);
}

static void write_bytes(HANDLE file, const void *data, DWORD length) {
    DWORD written;
    if (file != INVALID_HANDLE_VALUE && data && length) {
        WriteFile(file, data, length, &written, NULL);
    }
}

static DWORD descriptor_u32(const BYTE *descriptor, DWORD offset) {
    DWORD value = 0;
    memcpy(&value, descriptor + offset, sizeof(value));
    return value;
}

static const BYTE *descriptor_ptr(const BYTE *descriptor, DWORD offset) {
    DWORD value = descriptor_u32(descriptor, offset);
    return (const BYTE *)(UINT_PTR)value;
}

/* Exact interleaved or planar pixel span read by both Nikon builders. */
static DWORD builder_pixel_bytes(const BYTE *descriptor) {
    uint64_t width = descriptor_u32(descriptor, 0x00);
    uint64_t height = descriptor_u32(descriptor, 0x04);
    uint64_t stride = descriptor_u32(descriptor, 0x78);
    DWORD bits = descriptor_u32(descriptor, 0x7C);
    DWORD planar = descriptor_u32(descriptor, 0x80);
    uint64_t samples;
    uint64_t bytes;

    if (!width || !height || !stride || (bits != 8 && bits != 16)) return 0;
    if (planar == 1) {
        samples = (height - 1) * width * stride + 3 * width;
    } else {
        samples = (width * height - 1) * stride + 3;
    }
    bytes = samples * (bits == 8 ? 1 : 2);
    if (bytes > 512ULL * 1024ULL * 1024ULL) return 0;
    return (DWORD)bytes;
}

/*
 * One file per call to the selected scanner module's negative builder.
 *
 *   "NBLD" u32 version u32 sequence
 *   u32 descriptor_before_len, descriptor_before
 *   u32 meter_pixels_len, meter_pixels
 *   u32 characteristic_len, characteristic
 *   i32 result
 *   u32 descriptor_after_len, descriptor_after
 *   u32 lut_len, lut_bytes
 */
static int __cdecl hookedNikonBuilder(void *argument) {
    BYTE before[BUILDER_DESCRIPTOR_SIZE] = {0};
    BYTE after[BUILDER_DESCRIPTOR_SIZE] = {0};
    BYTE *descriptor = (BYTE *)argument;
    DWORD sequence = (DWORD)InterlockedIncrement(&g_builderSeq);
    DWORD descriptorLen = 0;
    DWORD pixelsLen = 0;
    DWORD characteristicLen = 0;
    DWORD lutLen = 0;
    const BYTE *pixels = NULL;
    const BYTE *characteristic = NULL;
    const BYTE *luts = NULL;
    char name[64];
    HANDLE file;
    int result;
    DWORD version = 1;

    if (descriptor && !IsBadReadPtr(descriptor, BUILDER_DESCRIPTOR_SIZE)) {
        memcpy(before, descriptor, BUILDER_DESCRIPTOR_SIZE);
        descriptorLen = BUILDER_DESCRIPTOR_SIZE;
        pixelsLen = builder_pixel_bytes(before);
        pixels = descriptor_ptr(before, 0x18);
        if (!pixels || (pixelsLen && IsBadReadPtr(pixels, pixelsLen))) pixelsLen = 0;
        characteristic = descriptor_ptr(before, 0x58);
        if (characteristic && !IsBadReadPtr(characteristic, 0x400)) {
            characteristicLen = 0x400;
        }
    }

    wsprintfA(name, "nikon_builder_%04lu.bin", sequence);
    file = open_capture(name, CREATE_ALWAYS);
    write_bytes(file, "NBLD", 4);
    write_bytes(file, &version, sizeof(version));
    write_bytes(file, &sequence, sizeof(sequence));
    write_bytes(file, &descriptorLen, sizeof(descriptorLen));
    write_bytes(file, before, descriptorLen);
    write_bytes(file, &pixelsLen, sizeof(pixelsLen));
    write_bytes(file, pixels, pixelsLen);
    write_bytes(file, &characteristicLen, sizeof(characteristicLen));
    write_bytes(file, characteristic, characteristicLen);

    result = g_realNikonBuilder ? g_realNikonBuilder(argument) : 3;

    if (descriptor && !IsBadReadPtr(descriptor, BUILDER_DESCRIPTOR_SIZE)) {
        memcpy(after, descriptor, BUILDER_DESCRIPTOR_SIZE);
        descriptorLen = BUILDER_DESCRIPTOR_SIZE;
        lutLen = descriptor_u32(after, 0x64);
        luts = descriptor_ptr(after, 0x68);
        if (lutLen > 16 * 1024 * 1024 || !luts || IsBadReadPtr(luts, lutLen)) lutLen = 0;
    } else {
        descriptorLen = 0;
    }
    write_bytes(file, &result, sizeof(result));
    write_bytes(file, &descriptorLen, sizeof(descriptorLen));
    write_bytes(file, after, descriptorLen);
    write_bytes(file, &lutLen, sizeof(lutLen));
    write_bytes(file, luts, lutLen);
    if (file != INVALID_HANDLE_VALUE) {
        FlushFileBuffers(file);
        CloseHandle(file);
    }

    log_write("Nikon builder capture %lu: result=%d pixels=%lu LUT=%lu\r\n",
              sequence, result, pixelsLen, lutLen);
    return result;
}

/* Replace the direct calls to the builder. The function itself remains
 * untouched, so the hook can call it without a trampoline. */
static void try_hook_builder(void) {
    HMODULE module;
    BYTE *base;
    BYTE *builder;
    size_t hooked = 0;

    if (g_builderHooked || g_builderAttempted) return;
    module = GetModuleHandleA(BUILDER_MODULE_NAME);
    if (!module) return;
    g_builderAttempted = TRUE;
    base = (BYTE *)module;
    builder = base + NIKON_BUILDER_RVA;
    g_realNikonBuilder = (pNikonBuilder)builder;

    for (size_t i = 0; i < sizeof(g_builderCallRvas) / sizeof(g_builderCallRvas[0]); i++) {
        BYTE *site = base + g_builderCallRvas[i];
        int32_t oldRel;
        int32_t newRel;
        BYTE *oldTarget;
        DWORD oldProtect;

        if (site[0] != 0xE8) continue;
        memcpy(&oldRel, site + 1, sizeof(oldRel));
        oldTarget = site + 5 + oldRel;
        if (oldTarget == (BYTE *)hookedNikonBuilder) {
            hooked++;
            continue;
        }
        if (oldTarget != builder) continue;
        newRel = (int32_t)((BYTE *)hookedNikonBuilder - (site + 5));
        if (!VirtualProtect(site, 5, PAGE_EXECUTE_READWRITE, &oldProtect)) continue;
        memcpy(site + 1, &newRel, sizeof(newRel));
        VirtualProtect(site, 5, oldProtect, &oldProtect);
        FlushInstructionCache(GetCurrentProcess(), site, 5);
        hooked++;
    }

    if (hooked == sizeof(g_builderCallRvas) / sizeof(g_builderCallRvas[0])) {
        g_builderHooked = TRUE;
        log_write("%s builder hook installed at 0x%08X\r\n",
                  BUILDER_TARGET_NAME, (DWORD)(UINT_PTR)builder);
    } else {
        log_write("ERROR: %s builder hook matched %d of %d call sites; no retry will be attempted\r\n",
                  BUILDER_TARGET_NAME,
                  (int)hooked,
                  (int)(sizeof(g_builderCallRvas) / sizeof(g_builderCallRvas[0])));
    }
}

/* Append one COMPLETE SCSI exchange to the binary trace — no truncation, so
 * 32 KB LUT writes and full image reads are captured intact (the text log's
 * log_hex still caps its line, which is why we need this).
 *
 * Record layout (all integers little-endian):
 *   "SREC"        4 bytes  magic
 *   seq           u32      correlates with the text log's [#N]
 *   cdb_len       u8
 *   direction     u8       0=none, 1=IN (device->host), 2=OUT (host->device)
 *   srb_status    u8
 *   sense_len     u8
 *   srb_flags     u32
 *   transfer_len  u32
 *   result        i32      DeviceIoControl BOOL return
 *   cdb           16 bytes
 *   data_len      u32
 *   data          data_len bytes  (the full SCSI data buffer)
 *   sense         sense_len bytes
 *
 * Parsed by the Rust nktrace/nkdiff tooling (see docs/PLAN.md). */
static void bin_write_scsi(DWORD seq, const SCSISCAN_CMD *cmd, BOOL result,
                           const BYTE *data, DWORD dataLen,
                           BYTE srbStatus, const BYTE *sense, DWORD senseLen)
{
    if (g_binFile == INVALID_HANDLE_VALUE) return;
    DWORD w;
    BYTE dir = (cmd->SrbFlags & SRB_FLAGS_DATA_IN)  ? 1 :
               (cmd->SrbFlags & SRB_FLAGS_DATA_OUT) ? 2 : 0;
    LONG  res = (LONG)result;
    BYTE  meta[4] = { cmd->CdbLength, dir, srbStatus, (BYTE)senseLen };

    WriteFile(g_binFile, "SREC", 4, &w, NULL);
    WriteFile(g_binFile, &seq, 4, &w, NULL);
    WriteFile(g_binFile, meta, 4, &w, NULL);
    WriteFile(g_binFile, &cmd->SrbFlags, 4, &w, NULL);
    WriteFile(g_binFile, &cmd->TransferLength, 4, &w, NULL);
    WriteFile(g_binFile, &res, 4, &w, NULL);
    WriteFile(g_binFile, cmd->Cdb, 16, &w, NULL);
    WriteFile(g_binFile, &dataLen, 4, &w, NULL);
    if (data && dataLen)   WriteFile(g_binFile, data, dataLen, &w, NULL);
    if (sense && senseLen) WriteFile(g_binFile, sense, senseLen, &w, NULL);
    FlushFileBuffers(g_binFile);
}

/* --- Hooked DeviceIoControl ---
 * Only installed in Nkdsbp2_real.dll's IAT, so only the real Nikon transport
 * DLL's DeviceIoControl calls are intercepted. Intercepts IOCTL_SCSISCAN_CMD
 * and dumps the CDB + data buffer (data-OUT before, data-IN after).
 *
 * With METHOD_OUT_DIRECT, the SCSI data buffer is lpOutBuffer (the driver
 * maps it via MDL for either direction). Direction comes from SrbFlags.
 *
 * Overlapped I/O handling: The Nikon driver issues ALL SCSI commands via
 * overlapped (async) I/O. DeviceIoControl returns FALSE (pending). Small
 * transfers complete fast enough that the buffer is filled when we read it.
 * Large transfers (image reads, 376KB-804KB) do NOT complete in time.
 * Fix: for data-IN commands, if the call returned FALSE and lpOverlapped is
 * provided, wait on the overlapped event before reading the buffer. We then
 * re-signal the event so the caller's own WaitForSingleObject also succeeds. */
static BOOL WINAPI hookedDeviceIoControl(
    HANDLE hDevice, DWORD dwIoControlCode,
    LPVOID lpInBuffer, DWORD nInBufferSize,
    LPVOID lpOutBuffer, DWORD nOutBufferSize,
    LPDWORD lpBytesReturned, LPOVERLAPPED lpOverlapped)
{
    BOOL result;
    DWORD savedErr = 0;
    DWORD seq = ++g_seq;

    if (dwIoControlCode == IOCTL_SCSISCAN_CMD &&
        lpInBuffer && nInBufferSize >= sizeof(SCSISCAN_CMD))
    {
        SCSISCAN_CMD *cmd = (SCSISCAN_CMD *)lpInBuffer;
        BOOL isDataIn  = (cmd->SrbFlags & SRB_FLAGS_DATA_IN)  != 0;
        BOOL isDataOut = (cmd->SrbFlags & SRB_FLAGS_DATA_OUT) != 0;

        log_write("[#%lu] --- IOCTL_SCSISCAN_CMD ---\r\n", seq);
        log_write("  CdbLength=%d SrbFlags=0x%08X (dir:%s) TransferLen=%d "
                  "InBufSize=%d OutBufSize=%d\r\n",
            cmd->CdbLength, cmd->SrbFlags,
            isDataIn ? "IN" : (isDataOut ? "OUT" : "none"),
            cmd->TransferLength, nInBufferSize, nOutBufferSize);
        log_hex("  CDB", cmd->Cdb, cmd->CdbLength);

        /* Data-OUT (host -> device, e.g. SET WINDOW): dump BEFORE the call.
         * The data is in lpOutBuffer (METHOD_OUT_DIRECT uses it for both
         * directions — the driver reads from or writes to this buffer). */
        if (isDataOut && cmd->TransferLength > 0 && lpOutBuffer) {
            int dumpLen = (int)cmd->TransferLength;
            if (dumpLen > 512) dumpLen = 512;
            log_hex("  DATA-OUT", (BYTE *)lpOutBuffer, dumpLen);
        }

        /* Call the real DeviceIoControl */
        result = realDeviceIoControl(hDevice, dwIoControlCode,
            lpInBuffer, nInBufferSize,
            lpOutBuffer, nOutBufferSize,
            lpBytesReturned, lpOverlapped);
        /* Capture last-error IMMEDIATELY — every Win32 call below (Sleep,
         * WriteFile, FlushFileBuffers, ...) resets the thread's last-error
         * as a side effect. The caller's overlapped pattern is
         * `if (!DeviceIoControl(...)) { if (GetLastError()==ERROR_IO_PENDING)
         * wait; else fail; }` — if anything overwrites it before we return,
         * the caller sees a bogus non-pending error on a perfectly good
         * pending I/O and treats it as a hard failure. This is what broke
         * device detection: every INQUIRY succeeded at the SCSI level but
         * Nikon Scan saw a clobbered error code and gave up. Restored right
         * before the return at the bottom of this function. */
        savedErr = GetLastError();

        /* For data-IN commands: wait for overlapped completion before reading.
         * The Nikon driver uses overlapped I/O for ALL SCSI commands —
         * DeviceIoControl returns FALSE (pending). Small transfers (INQUIRY,
         * GET WINDOW) complete fast enough that the buffer is already filled.
         * Large transfers (image reads) do NOT complete in time.
         *
         * Poll HasOverlappedIoCompleted() (reads OVERLAPPED.Internal, set
         * directly by the OS) instead of waiting on the completion event.
         * Waiting on the event ourselves would CONSUME it (auto-reset) or
         * require manually re-signaling it for the caller — either way we'd
         * be perturbing a synchronization object the caller also depends on.
         * Polling never touches it: the OS still signals the event/file
         * handle on its own, so the caller's own wait afterward sees it
         * completed normally, exactly as with the real DLL. */
        BOOL dataInComplete = result || !isDataIn || !lpOverlapped;
        if (PROXY_WAIT_FOR_DATA_IN && isDataIn && !result && lpOverlapped) {
            dataInComplete = TRUE;
            DWORD start = GetTickCount();
            while (!HasOverlappedIoCompleted(lpOverlapped)) {
                /* A thumbnail READ can take more than 10 s */
                if (GetTickCount() - start > 600000) {
                    log_write("  WARNING: overlapped read still pending after 600 s, buffer not captured\r\n");
                    dataInComplete = FALSE;
                    break;
                }
                Sleep(1);
            }
        }

        /* actualLen = the TRUE number of bytes the device wrote into
         * lpOutBuffer, which is NOT the same as cmd->TransferLength (the
         * requested size) on short transfers — e.g. the final chunk of an
         * image read, which legitimately ends with a CHECK CONDITION
         * SK=0x0B/ASC=0x4B short-transfer status. Trusting TransferLength
         * there means dumping/recording the tail of the buffer that the
         * device never wrote — exactly the zero-padding bug already found
         * (and fixed) on the Rust transport side. For an overlapped call
         * that completed (per the poll above), OVERLAPPED.InternalHigh
         * already holds the real transferred byte count — read it directly
         * instead of calling GetOverlappedResult() (which would reset
         * last-error again for no benefit). For a call that completed
         * synchronously, lpBytesReturned is already authoritative. */
        DWORD actualLen = cmd->TransferLength;
        if (isDataIn) {
            if (!result && lpOverlapped) {
                if (dataInComplete && HasOverlappedIoCompleted(lpOverlapped)) {
                    actualLen = (DWORD)lpOverlapped->InternalHigh;
                } else {
                    /* Do not read a buffer while the scanner can still be
                     * writing it. The caller remains responsible for waiting. */
                    actualLen = 0;
                }
            } else if (lpBytesReturned) {
                actualLen = *lpBytesReturned;
            }
        }

        /* Data-IN (device -> host, e.g. GET WINDOW / READ): dump AFTER.
         * By now the overlapped I/O has completed (we polled above). */
        if (isDataIn && actualLen > 0 && lpOutBuffer) {
            int dumpLen = (int)actualLen;
            if (dumpLen > 512) dumpLen = 512;
            log_hex("  DATA-IN", (BYTE *)lpOutBuffer, dumpLen);
        }

        /* Sense data: pSenseBuffer is a pointer to a buffer of SenseLength
         * bytes. Dereference it carefully. */
        if (cmd->pSenseBuffer && cmd->SenseLength > 0 &&
            !IsBadReadPtr(cmd->pSenseBuffer, cmd->SenseLength))
        {
            log_hex("  SENSE", cmd->pSenseBuffer, cmd->SenseLength);
        }

        /* SRB status: pSrbStatus points to a single status byte. */
        BYTE srbStatus = 0;
        if (cmd->pSrbStatus && !IsBadReadPtr(cmd->pSrbStatus, 1)) {
            srbStatus = *cmd->pSrbStatus;
            log_write("  SRB_Status=0x%02X\r\n", srbStatus);
        }

        /* Full untruncated binary record — read buffer inline.
         * By now the DMA has completed (we polled for it above). */
        {
            const BYTE *sensePtr = NULL;
            DWORD senseLen = 0;
            if (cmd->pSenseBuffer && cmd->SenseLength > 0 &&
                !IsBadReadPtr(cmd->pSenseBuffer, cmd->SenseLength)) {
                sensePtr = cmd->pSenseBuffer;
                senseLen = cmd->SenseLength;
            }

            /* isDataOut still uses cmd->TransferLength: that buffer was
             * prepared by the host itself before the call, so its full
             * requested length is always valid (no DMA race on writes). */
            DWORD outLen = isDataOut ? cmd->TransferLength : actualLen;
            const BYTE *dataPtr = NULL;
            DWORD dataLen = 0;
            if ((isDataIn || isDataOut) && outLen > 0 && lpOutBuffer &&
                !IsBadReadPtr(lpOutBuffer, outLen)) {
                dataPtr = (const BYTE *)lpOutBuffer;
                dataLen = outLen;
            }
            bin_write_scsi(seq, cmd, result, dataPtr, dataLen,
                           srbStatus, sensePtr, senseLen);
        }

        DWORD returned = isDataIn ? actualLen : (lpBytesReturned ? *lpBytesReturned : 0);
        log_write("  Result=%d BytesReturned=%d\r\n\r\n", result, returned);
    }
    else {
        /* Not our IOCTL — pass straight through, no logging noise. */
        result = realDeviceIoControl(hDevice, dwIoControlCode,
            lpInBuffer, nInBufferSize,
            lpOutBuffer, nOutBufferSize,
            lpBytesReturned, lpOverlapped);
        savedErr = GetLastError();
    }

    /* Restore the real call's last-error. The caller must still see
     * ERROR_IO_PENDING after our observation-only completion poll. */
    SetLastError(savedErr);
    return result;
}

/* --- IAT hook installation ---
 * Walks the import directory of hMod looking for kernel32!DeviceIoControl,
 * then overwrites that single IAT slot with our hook. Only hMod's calls are
 * affected — the rest of the process still calls the real DeviceIoControl. */
static void hookIatDeviceIoControl(HMODULE hMod) {
    HMODULE kernel32 = GetModuleHandleA("kernel32.dll");
    if (!kernel32) {
        log_write("WARN: kernel32.dll not in process — CDB capture disabled\r\n");
        return;
    }
    FARPROC realDio = GetProcAddress(kernel32, "DeviceIoControl");
    if (!realDio) {
        log_write("WARN: kernel32!DeviceIoControl not found — CDB capture disabled\r\n");
        return;
    }
    realDeviceIoControl = (pDeviceIoControl)realDio;
    log_write("kernel32!DeviceIoControl = 0x%08X\r\n", (DWORD)(UINT_PTR)realDio);

    BYTE *base = (BYTE *)hMod;
    IMAGE_DOS_HEADER *dos = (IMAGE_DOS_HEADER *)base;
    if (dos->e_magic != IMAGE_DOS_SIGNATURE) {
        log_write("WARN: bad DOS signature in real DLL\r\n"); return;
    }
    IMAGE_NT_HEADERS *nt = (IMAGE_NT_HEADERS *)(base + dos->e_lfanew);
    if (nt->Signature != IMAGE_NT_SIGNATURE) {
        log_write("WARN: bad PE signature in real DLL\r\n"); return;
    }

    DWORD impRva = nt->OptionalHeader.DataDirectory[IMAGE_DIRECTORY_ENTRY_IMPORT].VirtualAddress;
    if (!impRva) {
        log_write("WARN: real DLL has no import directory\r\n"); return;
    }

    IMAGE_IMPORT_DESCRIPTOR *imp = (IMAGE_IMPORT_DESCRIPTOR *)(base + impRva);
    int matchedDlls = 0;
    for (; imp->Name != 0; imp++) {
        const char *dllName = (const char *)(base + imp->Name);
        if (lstrcmpiA(dllName, "kernel32.dll") != 0) continue;
        matchedDlls++;

        /* FirstThunk is the IAT — already bound to real addresses by loader. */
        DWORD *iat = (DWORD *)(base + imp->FirstThunk);
        for (DWORD i = 0; iat[i] != 0; i++) {
            if ((FARPROC)iat[i] != realDio) continue;
            DWORD oldProt;
            if (!VirtualProtect(&iat[i], sizeof(DWORD), PAGE_READWRITE, &oldProt)) {
                log_write("WARN: VirtualProtect(IAT[%d]) failed=%lu\r\n", i, GetLastError());
                return;
            }
            iat[i] = (DWORD)(UINT_PTR)hookedDeviceIoControl;
            VirtualProtect(&iat[i], sizeof(DWORD), oldProt, &oldProt);
            /* Touch the page to flush any instruction cache staleness. */
            FlushInstructionCache(GetCurrentProcess(), &iat[i], sizeof(DWORD));
            log_write("IAT hook installed: kernel32!DeviceIoControl -> hooked (slot %d)\r\n", i);
            return;
        }
    }
    log_write("WARN: DeviceIoControl not found in real DLL IAT (matchedDlls=%d) — "
              "it may use GetProcAddress at runtime; CDB capture disabled\r\n",
              matchedDlls);
}

static BOOL buildRealDllPath(HINSTANCE hinstDLL, char *outPath, DWORD outPathSize);

/* Load and hook the real transport outside DllMain. LoadLibrary from DllMain
 * runs under the loader lock and can deadlock or terminate Nikon Scan. */
static BOOL ensure_initialized(void) {
    LONG state = InterlockedCompareExchange(&g_initState, 1, 0);
    if (state == 2) return TRUE;
    if (state == -1) return FALSE;
    if (state == 1) {
        do {
            Sleep(1);
            state = InterlockedCompareExchange(&g_initState, 1, 1);
        } while (state == 1);
        return state == 2;
    }

    {
        DWORD captureLen = GetEnvironmentVariableA(
            "NIKON_CAPTURE_DIR", g_captureDir, sizeof(g_captureDir));
        if (captureLen == 0 || captureLen >= sizeof(g_captureDir)) {
            lstrcpyA(g_captureDir, "C:\\");
        }
    }
    g_logFile = open_capture(PROXY_TRACE_PREFIX "_scsi_trace.log", CREATE_ALWAYS);
    g_binFile = open_capture(PROXY_TRACE_PREFIX "_scsi_trace.bin", CREATE_ALWAYS);
    log_write("=== %s proxy (complete SCSI capture) ===\r\n", PROXY_LABEL);
    log_write("Capture directory: %s\r\n", g_captureDir);
    log_write("SCSISCAN_CMD size=%d  IOCTL=0x%08X\r\n",
              (int)sizeof(SCSISCAN_CMD), (DWORD)IOCTL_SCSISCAN_CMD);

    {
        char path[MAX_PATH];
        if (buildRealDllPath(g_selfDll, path, sizeof(path))) {
            log_write("Real DLL: %s\r\n", path);
            g_realDll = LoadLibraryA(path);
        }
    }
    if (!g_realDll) g_realDll = LoadLibraryA(PROXY_REAL_DLL);
    if (!g_realDll) {
        log_write("ERROR: Could not load %s, GetLastError=%d\r\n",
                  PROXY_REAL_DLL, GetLastError());
        InterlockedExchange(&g_initState, -1);
        return FALSE;
    }
    log_write("Loaded real DLL at 0x%08X\r\n", (DWORD)(UINT_PTR)g_realDll);

    g_realNkDriverEntry = (pNkDriverEntry)GetProcAddress(g_realDll, "NkDriverEntry");
    if (!g_realNkDriverEntry)
        g_realNkDriverEntry = (pNkDriverEntry)GetProcAddress(g_realDll, (LPCSTR)1);
    if (!g_realNkDriverEntry) {
        log_write("ERROR: NkDriverEntry not found in real DLL\r\n");
        InterlockedExchange(&g_initState, -1);
        return FALSE;
    }
    log_write("NkDriverEntry: 0x%08X\r\n", (DWORD)(UINT_PTR)g_realNkDriverEntry);
    hookIatDeviceIoControl(g_realDll);
    log_write("=== Ready ===\r\n\r\n");
    InterlockedExchange(&g_initState, 2);
    return TRUE;
}

/* --- Exported NkDriverEntry (3 params, __stdcall) ---
 * Forwards to the real DLL and logs the high-level call for correlation with
 * the CDB trace below. */
__declspec(dllexport) int WINAPI NkDriverEntry(DWORD op, DWORD param2, DWORD param3)
{
    if (!ensure_initialized()) return -1;
    DWORD seq = ++g_seq;

    if (!PROXY_TRANSPORT_ONLY) try_hook_builder();

    if (g_logFile != INVALID_HANDLE_VALUE) {
        log_write("[#%lu] NkDriverEntry(op=%d, param2=0x%X, param3=0x%X)\r\n",
            seq, op, param2, param3);
        if (!PROXY_TRANSPORT_ONLY && param2 > 0x10000)
            log_hex("  param2", (BYTE *)param2, 64);
        if (!PROXY_TRANSPORT_ONLY && param3 > 0x10000 && param3 != param2)
            log_hex("  param3", (BYTE *)param3, 64);
    }

    if (!g_realNkDriverEntry) return -1;

    int result = g_realNkDriverEntry(op, param2, param3);
    if (g_logFile != INVALID_HANDLE_VALUE) {
        log_write("  -> result=%d\r\n", result);
        if (!PROXY_TRANSPORT_ONLY && op >= 2 && param3 > 0x10000)
            log_hex("  param3 (post)", (BYTE *)param3, 64);
    }
    return result;
}

/* --- DLL lifecycle --- */

static BOOL buildRealDllPath(HINSTANCE hinstDLL, char *outPath, DWORD outPathSize) {
    char path[MAX_PATH];
    DWORD len = GetModuleFileNameA(hinstDLL, path, MAX_PATH);
    if (len == 0 || len == MAX_PATH) return FALSE;
    char *slash = strrchr(path, '\\');
    if (!slash) return FALSE;
    *(slash + 1) = '\0';
    lstrcpynA(outPath, path, outPathSize);
    lstrcatA(outPath, PROXY_REAL_DLL);
    return TRUE;
}

BOOL WINAPI DllMain(HINSTANCE hinstDLL, DWORD fdwReason, LPVOID lpvReserved) {
    (void)lpvReserved;
    switch (fdwReason) {
    case DLL_PROCESS_ATTACH:
        g_selfDll = hinstDLL;
        DisableThreadLibraryCalls(hinstDLL);
        break;
    case DLL_PROCESS_DETACH:
        log_write("=== Proxy unloaded (seq=%lu) ===\r\n", g_seq);
        if (g_logFile != INVALID_HANDLE_VALUE) CloseHandle(g_logFile);
        if (g_binFile != INVALID_HANDLE_VALUE) CloseHandle(g_binFile);
        break;
    }
    return TRUE;
}
