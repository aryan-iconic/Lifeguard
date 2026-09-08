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

from datetime import datetime
from typing import Dict, Iterator, List, Optional, Set, Union
from facebook.employee import thrift_types
from facebook.employee.thrift_clients import EmployeeService
from facebook.employee.thrift_types import Employee
from facebook.team.thrift_clients import TeamService
from libfb.py._employee_caller_id import caller_id, CLIENT_ID_OVERRIDE_ENV  # noqa: F401
from libfb.py.decorators import memoize_multiple, memoize_timed, run_once
from libfb.py.pwdutils import get_current_user_name
from servicerouter.python.client_params import ClientParams
from servicerouter.python.sync_client import get_sr_client
