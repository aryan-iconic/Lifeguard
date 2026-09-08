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

import base64
from typing import Optional
from py3_asyncio.infrasec.authorization.acl.thrift_types import (
    CREWMATE,
    DATA_PROJECT,
    FBID,
    INTERN_AUTOMATION,
    SANDCASTLE_TAG,
    SERVICE_IDENTITY,
)
try:
    from crypto_auth_token_util import CryptoAuthTokenUtil
except ImportError:
    from corp_crypto_auth_token_util import CryptoAuthTokenUtil
from libfb.py.asyncio.await_utils import await_sync
from libfb.py.interngraph.auth.interngraph.thrift_types import (
    InternGraphCryptoAuthTokenPayload,
)
from py3_asyncio.cryptocat.common.common.thrift_types import TCryptoAuthToken
from py3_asyncio.cryptocat.common.common.types import TCryptoAuthTokenRuleList
from py3_asyncio.infrasec.authorization.acl.types import Identity
from thrift.python.protocol import Protocol
from thrift.python.serializer import deserialize, serialize
