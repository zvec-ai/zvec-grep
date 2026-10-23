# SDK read sessions

For repeated queries against an existing workspace index, open a read session
from the package entry point. It keeps the native index handles open until you
close it. The session exposes `context()` and `status()`.

```js
import { openWorkspaceReadSession } from "@zvec/zvec-grep";

const reader = openWorkspaceReadSession(process.cwd());
try {
  const status = await reader.status();
  const result = await reader.context({
    routes: [{ mode: "fts", query: "myFunction" }],
  });
  console.log({ filesIndexed: status.filesIndexed, hits: result.items });
} finally {
  await reader.close();
}
```

For vector or hybrid routes, pass an embedding model compatible with the
workspace index as the second argument. The caller owns that model and must
dispose it separately when finished.

The session selects the nearest indexed workspace when opened. It does not
automatically update the index. It holds a read lock for its whole lifetime, so
index, rebuild, disable, and drop operations on that workspace fail with a busy
lock until `close()` completes. Close the session before updating the index,
then open a new session to read the updated data. Calls started before `close()`
finish first; subsequent calls reject with `READ_SESSION_CLOSED`.
