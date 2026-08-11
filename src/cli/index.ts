#!/usr/bin/env node

import { parseArgs } from "./args.js";
import { colorModeFromArgs, printError } from "./errors.js";
import { printHelp } from "./help.js";
import { restartWithLiftoffOnlyIfNeeded } from "./runtime.js";
import { readPackageVersion } from "./version.js";

const PACKAGE_VERSION = readPackageVersion();

void main();

async function main(): Promise<void> {
  const args = process.argv.slice(2);
  const errorColor = colorModeFromArgs(args);

  try {
    const parsed = parseArgs(args);

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

    if (await restartWithLiftoffOnlyIfNeeded(parsed, args)) {
      return;
    }

    const { runParsedCommand } = await import("./commands.js");
    await runParsedCommand(parsed);
    process.exitCode = 0;
  } catch (error) {
    printError(error, { color: errorColor });
    process.exitCode = 1;
  }
}
