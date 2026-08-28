// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = markrust::run(&args);
    if code != 0 {
        std::process::exit(code);
    }
}
