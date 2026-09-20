#!/usr/bin/env -S node --liftoff-only

import { compatibilityWarningForArgs, parseArgs } from "./args.js";
import { runParsedCommand } from "./commands.js";
import { colorModeFromArgs, printError } from "./errors.js";
import { printHelp } from "./help.js";
import { readPackageVersion } from "./version.js";

const PACKAGE_VERSION = readPackageVersion();

void main();

async function main(): Promise<void> {
  const args = process.argv.slice(2);
  const errorColor = colorModeFromArgs(args);
  let debug = false;

  try {
    const compatibilityWarning = compatibilityWarningForArgs(args);
    if (compatibilityWarning) console.error(compatibilityWarning);

    const parsed = parseArgs(args);
    if (parsed.command === "query" && parsed.options.human === undefined) {
      parsed.options.human = process.stdout.isTTY === true;
    }
    debug = parsed.options.debug === true;

    if (parsed.command === "version") {
      console.log(PACKAGE_VERSION);
      process.exitCode = 0;
      return;
    }

    if (parsed.command === "help") {
      printHelp(PACKAGE_VERSION, parsed.helpTopic);
      process.exitCode = 0;
      return;
    }

    await runParsedCommand(parsed);
    process.exitCode = 0;
  } catch (error) {
    printError(error, { color: errorColor, debug });
    process.exitCode = 1;
  }
}
