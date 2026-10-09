import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { after, before } from "node:test";

// The host-local binding store lives in the global home; tests must not read
// or write the real one. Point ZVEC_GREP_HOME at a temporary directory for
// the duration of the file's tests.
export function useIsolatedZvecGrepHome() {
  let home;
  let previous;
  before(async () => {
    previous = process.env.ZVEC_GREP_HOME;
    home = await mkdtemp(join(tmpdir(), "zg-test-home-"));
    process.env.ZVEC_GREP_HOME = home;
  });
  after(async () => {
    if (previous === undefined) {
      delete process.env.ZVEC_GREP_HOME;
    } else {
      process.env.ZVEC_GREP_HOME = previous;
    }
    await rm(home, { recursive: true, force: true });
  });
}
