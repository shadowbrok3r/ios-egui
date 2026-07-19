// bindgen entry point: the QNN backend interface and system interface headers
// transitively include QnnTypes/Common/Context/Graph/Tensor/Device/System, which
// cover every type this crate needs (provider tables, binary-info, tensors, quant).
// HTP perf-infra (DCVS) headers are device-only and use C++ constructs; they are
// intentionally excluded and hand-stubbed in device.rs (host never runs them).
#include <stdbool.h>
#include <stdint.h>
#include "QnnInterface.h"
#include "System/QnnSystemInterface.h"
