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

from typing import (
    AbstractSet,
    Any,
    Callable,
    Dict,
    Generic,
    List,
    Mapping,
    Optional,
    Sequence,
    Set,
    Tuple,
    Type,
    TypeVar,
    Union,
)
from folly.iobuf import IOBuf
from ligen.clf.detail.ligen_clf_thrift_lib.thrift_types import IOBufStruct, UnitValue
from ligen.lib.py.types import JSON as JsonT
from pyre_extensions import TypeVarTuple, Unpack
from result import Result
from thrift.python.types import Enum, Struct, Union as ThriftUnionType
