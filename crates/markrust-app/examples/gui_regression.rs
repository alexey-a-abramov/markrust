// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// AppKit text shaping and Metal initialization must run on the main thread.
fn main() -> anyhow::Result<()> {
    markrust_app::visual_tests::run(std::env::args().skip(1))
}
