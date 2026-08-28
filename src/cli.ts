#!/usr/bin/env bun

import { runCli } from "./cli/main.ts";

const exitCode = await runCli(process.argv.slice(2));
process.exitCode = exitCode;
