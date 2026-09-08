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

import os
import re
import socket
import subprocess
from ipaddress import (
    _BaseAddress as _BaseIP,
    ip_address as IPAddress,
    IPv4Address,
    IPv6Address,
    IPv6Network,
)
from types import TracebackType
from typing import Dict, List, Optional, Tuple, Type, Union
import libfb.py.fbwhoami as fbwhoami
from libfb.py import getifaddrs
from libfb.py.decorators import memoize_timed
from libfb.py.fileutils import readfile
from libfb.py.pyre import none_throws
