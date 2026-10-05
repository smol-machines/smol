"""smol — embed isolated microVM sandboxes directly in your Python code.

Same API local (embedded engine, no server) or cloud (smolfleet) — the backend
is chosen via :class:`ConnectOptions` / ``SMOL_CLOUD_TOKEN``. Mirrors the Node SDK.

>>> from smol import Machine, MachineConfig
>>> with Machine.create(MachineConfig(image="python:3.12")) as m:    # doctest: +SKIP
...     print(m.run("python:3.12", ["python", "-c", "print(2 ** 10)"]).stdout)
"""

from __future__ import annotations

from .errors import (
    ExecutionError,
    InvalidConfigError,
    NotSupportedError,
    SmolError,
    wrap_native_error,
)
from .async_machine import AsyncEpisode, AsyncMachine
from .device_handoff import (
    DeviceAdapterBundle,
    DeviceAdapterServer,
    DeviceTensor,
    publish_device_adapter,
)
from .machine import Episode, Machine
from .transport import ExecStream, local_availability
from .rollout import RolloutClient, RolloutError, adapter_sha256
from .types import (
    ConnectOptions,
    EgressInterceptor,
    ExecOptions,
    ExecResult,
    ImageInfo,
    MachineConfig,
    MachineUsageReport,
    ShareLink,
    PortableCheckpointInfo,
    MachineResources,
    MountSpec,
    PortEndpoint,
    PortSpec,
    ResourceSpec,
)

__version__ = "1.23.1"

__all__ = [
    "Machine",
    "AsyncMachine",
    "Episode",
    "AsyncEpisode",
    "MachineConfig",
    "ResourceSpec",
    "MountSpec",
    "PortSpec",
    "PortEndpoint",
    "ExecOptions",
    "ExecResult",
    "ExecStream",
    "local_availability",
    "ImageInfo",
    "MachineUsageReport",
    "ShareLink",
    "PortableCheckpointInfo",
    "MachineResources",
    "ConnectOptions",
    "EgressInterceptor",
    "SmolError",
    "NotSupportedError",
    "InvalidConfigError",
    "ExecutionError",
    "wrap_native_error",
    "RolloutClient",
    "RolloutError",
    "adapter_sha256",
    "DeviceAdapterBundle",
    "DeviceAdapterServer",
    "DeviceTensor",
    "publish_device_adapter",
]
