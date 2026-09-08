# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# Licensed under the Apache License, Version 2.0 (the "License");
# you may not use this file except in compliance with the License.
# You may obtain a copy of the License at
#
#     http://www.apache.org/licenses/LICENSE-2.0
#
# Unless required by applicable law or agreed to in writing, software
# distributed under the License is distributed on an "AS IS" BASIS,
# WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
# See the License for the specific language governing permissions and
# limitations under the License.

import asyncio
import asyncio.runners
import contextvars
import functools
import inspect
import os
import random
import threading
import time
import types
from concurrent.futures import Future, ThreadPoolExecutor
from contextlib import AbstractAsyncContextManager, contextmanager
from typing import (
    Any,
    Awaitable,
    Callable,
    Coroutine,
    Iterator,
    Optional,
    ParamSpec,
    Tuple,
    Type,
    TypeVar,
)
from later import run_nested as wait_for  # noqa: F401
if hasattr(asyncio, 'Runner'):
    from libfb.py.asyncio.py312.magic import run as run, Runner as Runner  # noqa: F401
else:
    import asyncio.events
    from later.runner import get_running_loop, pause_existing_loop
