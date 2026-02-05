// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

use clap::Parser as _;
use litebox_runner_linux_arm64_userland::CliArgs;

fn main() -> anyhow::Result<()> {
    litebox_runner_linux_arm64_userland::run(CliArgs::parse())
}
