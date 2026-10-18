// Только проверка ABI при сборке: не новая реализация протокола и не shim.
#include <xipc/xipc.h>
_Static_assert(sizeof(xipc_status) == 4, "status ABI");
_Static_assert(sizeof(xipc_disconnect_reason) == 4, "reason ABI");
_Static_assert(sizeof(xipc_layout) == 24, "layout ABI");
_Static_assert(offsetof(xipc_channel_config, security) == 24, "security offset");
_Static_assert(XIPC_OK == 0 && XIPC_E_NO_MEMORY == 25, "status endpoints");
_Static_assert(XIPC_E_BUFFER_TOO_SMALL == 20 && XIPC_E_NOT_CONNECTED == 14, "session status ABI");
#if defined(_M_X64)
_Static_assert(sizeof(xipc_security) == 24, "security x64");
_Static_assert(sizeof(xipc_channel_config) == 56, "config x64");
_Static_assert(offsetof(xipc_channel_config, ns) == 48, "namespace x64");
#else
_Static_assert(sizeof(xipc_security) == 12, "security x86");
_Static_assert(sizeof(xipc_channel_config) == 40, "config x86");
_Static_assert(offsetof(xipc_channel_config, ns) == 36, "namespace x86");
#endif
