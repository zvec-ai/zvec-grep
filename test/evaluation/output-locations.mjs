// Extract returned source ranges, ignoring preview context lines. A containing
// symbol range is a location hit, not proof that its short excerpt answers the
// question. Evaluate snippet usefulness separately.
export function locations(stdout, files) {
  const hits = [];
  let file;
  const isFile = (path) => (files ? files.has(path) : /^src\//.test(path));
  for (const line of stdout.split("\n")) {
    const path = /^#(\d+) matchedBy=path (.+)$/.exec(line);
    if (path) {
      file = undefined;
      if (isFile(path[2])) hits.push({ rank: +path[1], file: path[2] });
      continue;
    }
    const indexed =
      /^#(\d+).*? matchedBy=\S+(?: score=\S+)? (.+):(\d+)(?:-(\d+))?$/.exec(
        line,
      );
    if (indexed) {
      file = undefined;
      if (isFile(indexed[2]))
        hits.push({
          rank: +indexed[1],
          file: indexed[2],
          start: +indexed[3],
          end: +(indexed[4] ?? indexed[3]),
        });
      continue;
    }
    if (isFile(line)) file = line;
    else if (line && !/^\s/.test(line)) file = undefined;
    const lexical = /^ {2}(\d+)(?:-(\d+))?(?: \[|:)/.exec(line);
    if (file && lexical) {
      hits.push({
        rank: hits.length + 1,
        file,
        start: +lexical[1],
        end: +(lexical[2] ?? lexical[1]),
      });
    }
  }
  return hits;
}
