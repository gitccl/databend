// Copyright 2021 Datafuse Labs.
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

use std::alloc::Layout;
use std::ptr::NonNull;

use bumpalo::Bump;

pub struct Area {
    bump: Bump,
}

impl Area {
    pub fn new() -> Area {
        Area { bump: Bump::new() }
    }

    pub fn alloc_layout(&mut self, layout: Layout) -> NonNull<u8> {
        self.bump.alloc_layout(layout)
    }
}

unsafe impl Send for Area {}
