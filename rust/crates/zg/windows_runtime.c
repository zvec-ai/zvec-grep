// MSVC delay-load hook: keep bin/zg.exe directly executable with DLLs in lib/.
// https://learn.microsoft.com/cpp/build/reference/understanding-the-helper-function
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <delayimp.h>
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
