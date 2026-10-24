// Wire protocol between the Scarlet-native client and the Widevine host
// process. The host runs inside a Linux ABI view; the client owns the control
// socket and the shared memory region and passes both at spawn time.
//
// Framing: 16-byte header { magic, kind, flags, length } followed by length
// bytes of payload. Every integer is little-endian.

#ifndef WIDEVINE_HOST_PROTOCOL_H_
#define WIDEVINE_HOST_PROTOCOL_H_

#include <stdint.h>

#define WVH_MAGIC 0x31564857u /* WVH1 */
#define WVH_VERSION 2u /* reserved input slot, video drain/status and deinitialize */

/* Client to host. */
enum {
  WVH_CMD_HELLO = 1,
  WVH_CMD_INIT = 2,
  WVH_CMD_CREATE_SESSION = 3,
  WVH_CMD_UPDATE_SESSION = 4,
  WVH_CMD_CLOSE_SESSION = 5,
  WVH_CMD_REMOVE_SESSION = 6,
  WVH_CMD_DECRYPT = 7,
  WVH_CMD_SHUTDOWN = 8,
  WVH_CMD_RELEASE = 9,
  WVH_CMD_CONFIGURE_VIDEO = 10,
  WVH_CMD_DECODE = 11,
  WVH_CMD_DEINITIALIZE_VIDEO = 13, /* request_id */
  WVH_CMD_SET_SERVER_CERTIFICATE = 12, /* request_id, certificate blob */
};

/* Host to client. */
enum {
  WVH_EV_READY = 100,
  WVH_EV_RESULT = 101,
  WVH_EV_SESSION = 102,
  WVH_EV_MESSAGE = 103,
  WVH_EV_KEYS = 104,
  WVH_EV_EXPIRATION = 105,
  WVH_EV_SESSION_CLOSED = 106,
  WVH_EV_FRAME = 107,
};

/* Result status shared by EV_RESULT and the native client. */
enum {
  WVH_OK = 0,
  WVH_NEED_MORE_DATA = 1, /* decode consumed input, no picture yet */
  WVH_ERROR = -1,
  WVH_NO_KEY = -2,
  WVH_DECRYPT_ERROR = -3,
  WVH_UNSUPPORTED = -4,
  WVH_TIMEOUT = -5,
};

/* Session and init data types mirror the CDM ABI. */
enum {
  WVH_SESSION_TEMPORARY = 0,
  WVH_SESSION_PERSISTENT_LICENSE = 1,
};

enum {
  WVH_INIT_DATA_CENC = 0,
  WVH_INIT_DATA_KEYIDS = 1,
  WVH_INIT_DATA_WEBM = 2,
};

enum {
  WVH_MESSAGE_LICENSE_REQUEST = 0,
  WVH_MESSAGE_LICENSE_RENEWAL = 1,
  WVH_MESSAGE_LICENSE_RELEASE = 2,
  WVH_MESSAGE_INDIVIDUALIZATION_REQUEST = 3,
};

/* Encryption schemes mirror ISO/IEC 23001-7 as exposed by the CDM ABI. */
enum {
  WVH_SCHEME_CENC = 1,
  WVH_SCHEME_CBCS = 2,
};

/* Shared region layout: a fixed 4096-byte header followed by slots. */
#define WVH_SHM_HEADER_BYTES 4096u

typedef struct {
  uint32_t magic;        /* WVH_MAGIC */
  uint32_t version;      /* WVH_VERSION */
  uint32_t header_bytes; /* WVH_SHM_HEADER_BYTES */
  uint32_t slot_count;
  uint32_t slot_size;
  uint32_t reserved[59];
} wvh_shm_header;

/* Payload layouts. All variable-length fields are { u32 size, bytes }. */
typedef struct {
  uint32_t client_version; /* CMD_HELLO */
} wvh_hello;

typedef struct {
  uint32_t host_version; /* EV_READY */
  uint32_t header_bytes;
  uint32_t slot_count;
  uint32_t slot_size;
} wvh_ready;

typedef struct {
  uint32_t allow_distinctive_identifier; /* CMD_INIT */
  uint32_t allow_persistent_state;
  uint32_t use_hw_secure_codecs;
} wvh_init;

typedef struct {
  uint32_t request_id; /* EV_RESULT */
  int32_t status;
  uint32_t value0; /* decrypt: output slot */
  uint32_t value1; /* decrypt: output size */
} wvh_result;

typedef struct {
  uint32_t request_id;    /* CMD_CREATE_SESSION */
  uint32_t session_type;
  uint32_t init_data_type;
} wvh_create_session;

typedef struct {
  uint32_t request_id; /* CMD_UPDATE_SESSION, CMD_CLOSE_SESSION, CMD_REMOVE_SESSION */
} wvh_promise_request;

typedef struct {
  uint32_t session_id_size; /* EV_SESSION, EV_SESSION_CLOSED */
} wvh_session_event;

typedef struct {
  uint32_t request_id; /* EV_FRAME */
  uint32_t format;
  uint32_t width;
  uint32_t height;
  uint32_t slot;
  uint32_t size;
  uint32_t stride_y;
  uint32_t stride_u;
  uint32_t stride_v;
  uint32_t offset_u;
  uint32_t offset_v;
  int64_t timestamp;
} wvh_frame_event;

typedef struct {
  uint32_t session_id_size; /* EV_MESSAGE */
  uint32_t message_type;
  uint32_t slot;
  uint32_t offset;
  uint32_t size;
} wvh_message_event;

typedef struct {
  uint32_t codec;   /* CMD_CONFIGURE_VIDEO, CDM VideoCodec */
  uint32_t profile; /* CDM VideoCodecProfile */
  uint32_t format;  /* CDM VideoFormat */
  uint32_t coded_width;
  uint32_t coded_height;
} wvh_video_config;

typedef struct {
  uint32_t request_id; /* CMD_DECRYPT, CMD_DECODE */
  uint32_t slot;
  uint32_t offset;
  uint32_t size;
  uint32_t key_id_size;
  uint32_t iv_size;
  uint32_t subsample_count;
  uint32_t scheme; /* WVH_SCHEME_* */
  uint32_t release_slot; /* slot to release after the request */
  uint32_t reserved;
  int64_t timestamp;
} wvh_decrypt;
#endif /* WIDEVINE_HOST_PROTOCOL_H_ */
