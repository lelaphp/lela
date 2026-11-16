// SPDX-License-Identifier: MIT AND BSD-3-Clause
// Copyright © The PHP Group and Contributors.
// Copyright © Zend Technologies Ltd., a subsidiary company of Perforce Software, Inc.
// Derived in part from php-src, used under BSD-3-Clause: see NOTICE.

//! Three files that state an expression: `alone` says `MIT` and no list
//! names it, `wide` adds a licence no row gives it, and `listed` carries
//! the four lines, which fails once a list names it.

pub mod alone;
pub mod listed;
pub mod wide;
