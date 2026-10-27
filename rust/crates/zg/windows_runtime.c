// MSVC delay-load hook: keep bin/zg.exe directly executable with DLLs in lib/.
// https://learn.microsoft.com/cpp/build/reference/understanding-the-helper-function
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <delayimp.h>
#include <shlwapi.h>
#include <wchar.h>

static INIT_ONCE runtime_once = INIT_ONCE_STATIC_INIT;
static wchar_t runtime_directory[32768];

static BOOL CALLBACK configure_runtime(PINIT_ONCE once, PVOID parameter, PVOID *context) {
    (void)once;
    (void)parameter;
    (void)context;
    DWORD length = GetModuleFileNameW(NULL, runtime_directory, 32768);
    if (length == 0 || length >= 32768) {
        runtime_directory[0] = L'\0';
        return TRUE;
    }
    wchar_t *separator = wcsrchr(runtime_directory, L'\\');
    if (separator == NULL) {
        runtime_directory[0] = L'\0';
        return TRUE;
    }
    *separator = L'\0';
    // A release package has bin/ and lib/. Cargo's development tree instead
    // stages DLLs beside the executable, so leave its normal lookup intact.
    if (wcscat_s(runtime_directory, 32768, L"\\..\\lib") != 0) {
        runtime_directory[0] = L'\0';
        return TRUE;
    }
    DWORD attributes = GetFileAttributesW(runtime_directory);
    if (attributes == INVALID_FILE_ATTRIBUTES || !(attributes & FILE_ATTRIBUTE_DIRECTORY)) {
        runtime_directory[0] = L'\0';
        return TRUE;
    }
    // Also used by llama.cpp plugins and ORT's dynamically loaded providers.
    // Keep the system/PATH search for NVIDIA's host-provided runtime libraries.
    if (!SetDllDirectoryW(runtime_directory)) {
        runtime_directory[0] = L'\0';
    }
    return TRUE;
}

static void configure_custom_ort_directory(void) {
    wchar_t library[32768];
    DWORD length = GetEnvironmentVariableW(L"ORT_DYLIB_PATH", library, 32768);
    if (length == 0) {
        length = GetEnvironmentVariableW(L"ZVEC_GREP_NATIVE_LIB_DIR", library, 32768);
        if (length == 0 || length >= 32768 ||
            wcscat_s(library, 32768, L"\\onnxruntime.dll") != 0) {
            return;
        }
    } else if (length >= 32768) {
        return;
    }

    // Match ort's relative-path lookup: executable-relative when that file
    // exists, otherwise relative to the process's working directory.
    if (PathIsRelativeW(library)) {
        wchar_t adjacent[32768];
        length = GetModuleFileNameW(NULL, adjacent, 32768);
        if (length > 0 && length < 32768) {
            wchar_t *separator = wcsrchr(adjacent, L'\\');
            if (separator != NULL) {
                separator[1] = L'\0';
                if (wcscat_s(adjacent, 32768, library) == 0 &&
                    GetFileAttributesW(adjacent) != INVALID_FILE_ATTRIBUTES) {
                    wcscpy_s(library, 32768, adjacent);
                }
            }
        }
    }
    wchar_t absolute[32768];
    wchar_t *filename = NULL;
    length = GetFullPathNameW(library, 32768, absolute, &filename);
    if (length == 0 || length >= 32768 || filename == NULL) {
        return;
    }
    *filename = L'\0';
    // Loading an absolute ORT path does not add its parent to dependent-DLL
    // lookup. Prefer its own siblings (e.g. DirectML.dll), retaining system/PATH
    // lookup for NVIDIA runtime libraries. Packaged zvec/llama imports continue
    // using the explicit runtime_directory path in the delay-load hook below.
    SetDllDirectoryW(absolute);
}

// ORT now loads dynamically on every target. Configure sibling dependencies
// (notably DirectML.dll) before Rust can load ORT, even if no delayed zvec import
// has run yet. Cargo tests keep DLLs next to their own executable as before.
static void __cdecl initialize_runtime_directory(void) {
    InitOnceExecuteOnce(&runtime_once, configure_runtime, NULL, NULL);
    configure_custom_ort_directory();
}
#pragma section(".CRT$XCU", read)
__declspec(allocate(".CRT$XCU")) void (__cdecl *zg_runtime_initializer)(void) = initialize_runtime_directory;

static FARPROC WINAPI load_runtime(unsigned notification, PDelayLoadInfo info) {
    if (notification != dliNotePreLoadLibrary) {
        return NULL;
    }
    InitOnceExecuteOnce(&runtime_once, configure_runtime, NULL, NULL);
    if (runtime_directory[0] == L'\0') {
        return NULL;
    }
    wchar_t path[32768];
    wchar_t filename[256];
    if (MultiByteToWideChar(CP_UTF8, MB_ERR_INVALID_CHARS, info->szDll, -1,
                           filename, 256) == 0 ||
        wcscpy_s(path, 32768, runtime_directory) != 0 ||
        wcscat_s(path, 32768, L"\\") != 0 ||
        wcscat_s(path, 32768, filename) != 0) {
        return NULL;
    }
    return (FARPROC)LoadLibraryExW(path, NULL, LOAD_WITH_ALTERED_SEARCH_PATH);
}

const PfnDliHook __pfnDliNotifyHook2 = load_runtime;
