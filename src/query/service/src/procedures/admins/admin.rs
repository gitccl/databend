// Copyright 2021 Datafuse Labs
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use super::tenant_quota::TenantQuotaProcedure;
use crate::procedures::admins::license_info::LicenseInfoProcedure;
use crate::procedures::ProcedureFactory;

pub struct AdminProcedure;

impl AdminProcedure {
    pub fn register(factory: &mut ProcedureFactory) {
        factory.register(
            "admin$tenant_quota",
            Box::new(TenantQuotaProcedure::try_create),
        );
        factory.register(
            "admin$license_info",
            Box::new(LicenseInfoProcedure::try_create),
        );
    }
}
