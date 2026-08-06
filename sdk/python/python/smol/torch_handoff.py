"""Optional CUDA-VMM importer that exposes a device adapter as PyTorch tensors."""

from __future__ import annotations

import ctypes
import gc
from typing import Any

from .device_handoff import DeviceAdapterBundle


_CUDA_ARRAY_TYPES = {
    "bool": ("|b1", None),
    "int8": ("|i1", None),
    "uint8": ("|u1", None),
    "int16": ("<i2", None),
    "int32": ("<i4", None),
    "int64": ("<i8", None),
    "float16": ("<f2", None),
    "bfloat16": ("<u2", "bfloat16"),
    "float32": ("<f4", None),
    "float64": ("<f8", None),
    "float8_e4m3fn": ("|u1", "float8_e4m3fn"),
    "float8_e5m2": ("|u1", "float8_e5m2"),
}


class _CudaArray:
    def __init__(self, pointer: int, shape: tuple[int, ...], typestr: str) -> None:
        self.__cuda_array_interface__ = {
            "shape": shape,
            "strides": None,
            "typestr": typestr,
            "data": (pointer, False),
            "version": 3,
        }


class _CUmemLocation(ctypes.Structure):
    _fields_ = [("type", ctypes.c_int), ("id", ctypes.c_int)]


class _CUmemAccessDesc(ctypes.Structure):
    _fields_ = [("location", _CUmemLocation), ("flags", ctypes.c_uint64)]


class _CudaDriver:
    def __init__(self, device: int) -> None:
        self.library = ctypes.CDLL("libcuda.so.1")
        self.device = device
        self._bind()
        self.check(self.library.cuInit(0), "cuInit")

    def _bind(self) -> None:
        self.library.cuInit.argtypes = [ctypes.c_uint]
        self.library.cuMemImportFromShareableHandle.argtypes = [
            ctypes.POINTER(ctypes.c_uint64),
            ctypes.c_void_p,
            ctypes.c_int,
        ]
        self.library.cuMemAddressReserve.argtypes = [
            ctypes.POINTER(ctypes.c_uint64),
            ctypes.c_size_t,
            ctypes.c_size_t,
            ctypes.c_uint64,
            ctypes.c_uint64,
        ]
        self.library.cuMemMap.argtypes = [
            ctypes.c_uint64,
            ctypes.c_size_t,
            ctypes.c_size_t,
            ctypes.c_uint64,
            ctypes.c_uint64,
        ]
        self.library.cuMemSetAccess.argtypes = [
            ctypes.c_uint64,
            ctypes.c_size_t,
            ctypes.POINTER(_CUmemAccessDesc),
            ctypes.c_size_t,
        ]
        self.library.cuMemUnmap.argtypes = [ctypes.c_uint64, ctypes.c_size_t]
        self.library.cuMemAddressFree.argtypes = [ctypes.c_uint64, ctypes.c_size_t]
        self.library.cuMemRelease.argtypes = [ctypes.c_uint64]

    @staticmethod
    def check(status: int, operation: str) -> None:
        if status:
            raise RuntimeError(f"{operation} failed with CUDA status {status}")

    def import_allocation(self, descriptor: int, size: int) -> tuple[int, int]:
        handle = ctypes.c_uint64()
        self.check(
            self.library.cuMemImportFromShareableHandle(
                ctypes.byref(handle), ctypes.c_void_p(descriptor), 1
            ),
            "cuMemImportFromShareableHandle",
        )
        pointer = ctypes.c_uint64()
        try:
            self.check(
                self.library.cuMemAddressReserve(
                    ctypes.byref(pointer), size, 0, 0, 0
                ),
                "cuMemAddressReserve",
            )
            mapped = False
            try:
                self.check(
                    self.library.cuMemMap(pointer.value, size, 0, handle.value, 0),
                    "cuMemMap",
                )
                mapped = True
                access = _CUmemAccessDesc(
                    location=_CUmemLocation(type=1, id=self.device), flags=1
                )
                self.check(
                    self.library.cuMemSetAccess(
                        pointer.value, size, ctypes.byref(access), 1
                    ),
                    "cuMemSetAccess",
                )
            except Exception:
                if mapped:
                    self.library.cuMemUnmap(pointer.value, size)
                self.library.cuMemAddressFree(pointer.value, size)
                raise
        except Exception:
            self.library.cuMemRelease(handle.value)
            raise
        return handle.value, pointer.value

    def unmap(self, pointer: int, size: int) -> None:
        self.check(self.library.cuMemUnmap(pointer, size), "cuMemUnmap")

    def free_address(self, pointer: int, size: int) -> None:
        self.check(self.library.cuMemAddressFree(pointer, size), "cuMemAddressFree")

    def release_handle(self, handle: int) -> None:
        self.check(self.library.cuMemRelease(handle), "cuMemRelease")


class TorchDeviceAdapter:
    """Imported mapping, tensor views, and configuration for one LoRA version."""

    def __init__(self, bundle: DeviceAdapterBundle) -> None:
        import torch

        self.adapter_config: dict[str, Any] = bundle.manifest["adapterConfig"]
        self._driver = _CudaDriver(torch.cuda.current_device())
        self._size = bundle.allocation_size
        self._handle, self._pointer = self._driver.import_allocation(
            bundle.descriptor, self._size
        )
        self._mapped = True
        self._arrays = []
        self.tensors = {}
        try:
            for tensor in bundle.tensors:
                typestr, view_dtype = _CUDA_ARRAY_TYPES[tensor.dtype]
                array = _CudaArray(self._pointer + tensor.offset, tensor.shape, typestr)
                value = torch.as_tensor(array, device=f"cuda:{self._driver.device}")
                if view_dtype is not None:
                    value = value.view(getattr(torch, view_dtype))
                if value.data_ptr() != self._pointer + tensor.offset:
                    raise RuntimeError(
                        f"PyTorch copied imported tensor {tensor.name!r} instead of viewing it"
                    )
                if value.numel() * value.element_size() != tensor.size:
                    raise ValueError(
                        f"imported tensor {tensor.name!r} does not match its range"
                    )
                self._arrays.append(array)
                self.tensors[tensor.name] = value
        except Exception:
            self.close()
            raise

    def close(self) -> None:
        """Release views and the imported mapping after framework unload."""

        if not getattr(self, "_handle", 0):
            return
        import torch

        with torch.cuda.device(self._driver.device):
            torch.cuda.synchronize(self._driver.device)
            self.tensors.clear()
            self._arrays.clear()
            gc.collect()
            if self._mapped:
                self._driver.unmap(self._pointer, self._size)
                self._mapped = False
            if self._pointer:
                pointer = self._pointer
                self._driver.free_address(pointer, self._size)
                self._pointer = 0
            if self._handle:
                handle = self._handle
                self._driver.release_handle(handle)
                self._handle = 0


def import_torch_device_adapter(bundle: DeviceAdapterBundle) -> TorchDeviceAdapter:
    """Import a smolvm allocation and return named PyTorch tensor views."""

    return TorchDeviceAdapter(bundle)
