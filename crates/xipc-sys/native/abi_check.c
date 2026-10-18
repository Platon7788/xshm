// Только проверка ABI при сборке: не новая реализация протокола и не shim.
#include <xipc/xipc.h>
_Static_assert(XIPC_VERSION_MAJOR == 2 && XIPC_VERSION_MINOR >= 1, "xipc 2.1+ owned objects required");
_Static_assert(sizeof(xipc_status) == 4, "status ABI");
_Static_assert(sizeof(xipc_disconnect_reason) == 4, "reason ABI");
_Static_assert(sizeof(xipc_layout) == 24, "layout ABI");
_Static_assert(offsetof(xipc_channel_config, security) == 24, "security offset");
_Static_assert(XIPC_OK == 0 && XIPC_E_NO_MEMORY == 25, "status endpoints");
_Static_assert(XIPC_E_BUFFER_TOO_SMALL == 20 && XIPC_E_NOT_CONNECTED == 14, "session status ABI");
_Static_assert(sizeof(xipc_multi_event) == 20, "multi event ABI");
_Static_assert(XIPC_MEV_CONNECT == 1 && XIPC_MEV_ERROR == 5, "multi event kinds");
_Static_assert(sizeof(xipc_registration) == 72 && offsetof(xipc_registration, name) == 7, "registration ABI");
_Static_assert(offsetof(xipc_dispatch_options, channel_layout) == 36, "dispatch layout ABI");
_Static_assert(XIPC_DEV_CLIENT_CONNECT == 1 && XIPC_DEV_ERROR == 5, "dispatch event kinds");
#if defined(_M_X64)
_Static_assert(sizeof(xipc_client_wait_options) == 32 && offsetof(xipc_client_wait_options, security) == 16, "single wait x64");
_Static_assert(sizeof(xipc_dispatch_options) == 112 && sizeof(xipc_dispatch_client_options) == 48, "dispatch options x64");
_Static_assert(sizeof(xipc_dispatch_event) == 32 && offsetof(xipc_dispatch_event, reg) == 24, "dispatch event x64");
_Static_assert(offsetof(xipc_dispatch_options, admit) == 96, "admit x64");
_Static_assert(sizeof(xipc_multi_options) == 64, "multi options x64");
_Static_assert(sizeof(xipc_multi_client_options) == 32, "multi client x64");
_Static_assert(offsetof(xipc_multi_client_options, security) == 24, "multi security x64");
_Static_assert(sizeof(xipc_security) == 24, "security x64");
_Static_assert(sizeof(xipc_channel_config) == 56, "config x64");
_Static_assert(offsetof(xipc_channel_config, ns) == 48, "namespace x64");
#else
_Static_assert(sizeof(xipc_client_wait_options) == 20 && offsetof(xipc_client_wait_options, security) == 12, "single wait x86");
_Static_assert(sizeof(xipc_dispatch_options) == 84 && sizeof(xipc_dispatch_client_options) == 28, "dispatch options x86");
_Static_assert(sizeof(xipc_dispatch_event) == 24 && offsetof(xipc_dispatch_event, reg) == 20, "dispatch event x86");
_Static_assert(offsetof(xipc_dispatch_options, admit) == 76, "admit x86");
_Static_assert(sizeof(xipc_multi_options) == 44, "multi options x86");
_Static_assert(sizeof(xipc_multi_client_options) == 20, "multi client x86");
_Static_assert(offsetof(xipc_multi_client_options, security) == 16, "multi security x86");
_Static_assert(sizeof(xipc_security) == 12, "security x86");
_Static_assert(sizeof(xipc_channel_config) == 40, "config x86");
_Static_assert(offsetof(xipc_channel_config, ns) == 36, "namespace x86");
#endif
