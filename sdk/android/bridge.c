#include <jni.h>
#include <stdint.h>

extern char *rpc_start(const char *, uint64_t, const char *);
extern char *rpc_playback_url(const char *);
extern void rpc_close(void);
extern void rpc_free_string(char *);

static jstring java_string(JNIEnv *env, char *value) {
    if (!value) return NULL;
    jstring result = (*env)->NewStringUTF(env, value);
    rpc_free_string(value);
    return result;
}

JNIEXPORT jstring JNICALL Java_cn_gavinliu_rsproxycache_ProxyCache_nativeStart(
        JNIEnv *env, jclass type, jbyteArray directory, jlong capacity, jbyteArray hosts) {
    jbyte *path = (*env)->GetByteArrayElements(env, directory, NULL);
    if (!path) return NULL;
    jbyte *list = (*env)->GetByteArrayElements(env, hosts, NULL);
    char *error = NULL;
    if (list) {
        error = rpc_start((const char *)path, (uint64_t)capacity, (const char *)list);
        (*env)->ReleaseByteArrayElements(env, hosts, list, JNI_ABORT);
    }
    (*env)->ReleaseByteArrayElements(env, directory, path, JNI_ABORT);
    return java_string(env, error);
}

JNIEXPORT jstring JNICALL Java_cn_gavinliu_rsproxycache_ProxyCache_nativePlaybackUrl(
        JNIEnv *env, jclass type, jbyteArray source) {
    jbyte *bytes = (*env)->GetByteArrayElements(env, source, NULL);
    if (!bytes) return NULL;
    char *url = rpc_playback_url((const char *)bytes);
    (*env)->ReleaseByteArrayElements(env, source, bytes, JNI_ABORT);
    return java_string(env, url);
}

JNIEXPORT void JNICALL Java_cn_gavinliu_rsproxycache_ProxyCache_close(JNIEnv *env, jclass type) {
    rpc_close();
}
