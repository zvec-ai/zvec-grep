#!/usr/bin/env node

import { parseArgs } from "./args.js";
import { runParsedCommand } from "./commands.js";
import { colorModeFromArgs, printError } from "./errors.js";
import { printHelp } from "./help.js";
import { readPackageVersion } from "./version.js";

const PACKAGE_VERSION = readPackageVersion();

void main();

async function main(): Promise<void> {
  const args = process.argv.slice(2);
  const errorColor = colorModeFromArgs(args);

  try {
    const parsed = parseArgs(args);

    if (parsed.options.version) {
      console.log(PACKAGE_VERSION);
      process.exitCode = 0;
      return;
    }

    if (parsed.options.help || args.length === 0) {
      printHelp(PACKAGE_VERSION);
      process.exitCode = args.length === 0 ? 1 : 0;
      return;
    }

    await runParsedCommand(parsed);
    process.exitCode = 0;
  } catch (error) {
    printError(error, { color: errorColor });
    process.exitCode = 1;
  }
}
