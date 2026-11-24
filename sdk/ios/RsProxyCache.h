#pragma once
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* NULL means success. Release any returned error with rpc_free_string. */
char * _Nullable rpc_start(const char * _Nonnull directory, uint64_t capacity,
                           const char * _Nonnull hosts);
/* NULL means the proxy is unavailable or the source is invalid or disallowed. */
char * _Nullable rpc_playback_url(const char * _Nonnull source);
/* Download calls return JSON owned by the caller. Free it with rpc_free_string. */
/* Pass zero bandwidth to select the default HLS variant. */
char * _Nullable rpc_download_start(const char * _Nonnull source, uint64_t bandwidth);
char * _Nullable rpc_download_progress(const char * _Nonnull source);
char * _Nullable rpc_download_cancel(const char * _Nonnull source);
void rpc_close(void);
void rpc_free_string(char * _Nullable value);

#ifdef __cplusplus
}
#endif
