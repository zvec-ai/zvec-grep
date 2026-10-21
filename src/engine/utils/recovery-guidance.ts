/** Operator recovery must preserve the incomplete result and its barriers. */
export const INCOMPLETE_RECOVERY_HINT =
  "Stop all users of this index and verify the recorded owner on its host. Keep the source and backups. After all owners have exited, preserve the entire incomplete destination index home under a new quarantine path, then retry into an empty destination. Do not remove only a lock or the INCOMPLETE marker. See docs/09-portable-indexes.md.";
