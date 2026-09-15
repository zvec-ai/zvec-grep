import { chargeGlobWork, checkGlobLength } from "./glob-budget.js";

type State =
  | { kind: "accept" }
  | { kind: "split"; next: number[] }
  | { kind: "consume"; test: string | RegExp; next: number };

type Matcher = { test: (path: string) => boolean; weight: number };

const MAX_NESTING = 32;
const MAX_MATCH_WORK = 1_000_000;
const MAX_CACHE_WEIGHT = 131_072;
const MAX_CACHE_ENTRIES = 256;
const cache = new Map<string, Matcher>();
let cacheWeight = 0;

/** A bounded Thompson NFA: never retry a state at the same input position. */
export function compileGlob(
  pattern: string,
  caseInsensitive: boolean,
): Matcher {
  checkGlobLength(pattern, "pattern");
  const key = `${caseInsensitive ? "i" : "s"}\0${pattern}`;
  const cached = cache.get(key);
  if (cached) {
    cache.delete(key);
    cache.set(key, cached);
    return cached;
  }

  const states: State[] = [{ kind: "accept" }];
  const add = (state: State): number => states.push(state) - 1;
  const character = (value: string): string | RegExp =>
    caseInsensitive ? new RegExp(`^${escapeRegExp(value)}$`, "i") : value;
  const consume = (test: string | RegExp, next: number): number =>
    add({ kind: "consume", test, next });
  const star = (test: RegExp, next: number): number => {
    const split = add({ kind: "split", next: [] });
    states[split] = { kind: "split", next: [consume(test, split), next] };
    return split;
  };
  const directoryPrefix = (next: number): number =>
    add({ kind: "split", next: [star(/^.$/, consume("/", next)), next] });

  type Part = (next: number) => number;
  function fragment(value: string, next: number, depth: number): number {
    if (depth > MAX_NESTING) {
      throw new Error(
        `Glob pattern exceeds the ${MAX_NESTING}-level nesting limit.`,
      );
    }
    chargeGlobWork(value.length);
    const alternativesByStart = findAlternatives(value);
    const closingBrackets = new Int32Array(value.length + 1);
    closingBrackets[value.length] = -1;
    for (let index = value.length - 1; index >= 0; index--) {
      closingBrackets[index] =
        value[index] === "]" ? index : closingBrackets[index + 1];
    }
    const parts: Part[] = [];
    for (let index = 0; index < value.length; index++) {
      const char = value[index];
      if (char === "*" && value[index + 1] === "*") {
        if (value[index + 2] === "/") {
          parts.push(directoryPrefix);
          index += 2;
        } else {
          parts.push((tail) => star(/^.$/, tail));
          index++;
        }
      } else if (char === "*") {
        parts.push((tail) => star(/^[^/]$/, tail));
      } else if (char === "?") {
        parts.push((tail) => consume(/^[^/]$/, tail));
      } else if (char === "[") {
        const characterClass = readCharacterClass(
          value,
          index,
          closingBrackets[index + 1],
          caseInsensitive,
        );
        if (characterClass) {
          parts.push((tail) => consume(characterClass.test, tail));
          index = characterClass.end;
        } else {
          parts.push((tail) => consume(character("["), tail));
        }
      } else if (char === "{") {
        const alternatives = alternativesByStart.get(index);
        if (alternatives) {
          parts.push((tail) =>
            add({
              kind: "split",
              next: alternatives.values.map((part) =>
                fragment(part, tail, depth + 1),
              ),
            }),
          );
          index = alternatives.end;
        } else {
          parts.push((tail) => consume(character("{"), tail));
        }
      } else {
        parts.push((tail) => consume(character(char), tail));
      }
    }
    for (let index = parts.length - 1; index >= 0; index--) {
      next = parts[index](next);
    }
    return next;
  }

  let start = fragment(pattern, 0, 0);
  if (!pattern.includes("/")) start = directoryPrefix(start);

  const matcher: Matcher = {
    weight: pattern.length + states.length,
    test(path) {
      checkGlobLength(path, "path");
      const visited = new Uint32Array(states.length);
      let generation = 0;
      let work = 0;
      const stack: number[] = [];
      function expand(id: number, target: number[]): void {
        stack.push(id);
        while (stack.length > 0) {
          const current = stack.pop()!;
          if (visited[current] === generation) continue;
          visited[current] = generation;
          if (++work > MAX_MATCH_WORK) {
            throw new Error("Glob pattern exceeded its matching work limit.");
          }
          const state = states[current];
          if (state.kind === "split") stack.push(...state.next);
          else target.push(current);
        }
      }
      try {
        let current: number[] = [];
        let next: number[] = [];
        generation++;
        expand(start, current);
        // Index UTF-16 code units, like the previous non-Unicode RegExp matcher.
        for (let offset = 0; offset < path.length; offset++) {
          const char = path[offset];
          generation++;
          next.length = 0;
          for (const id of current) {
            const state = states[id];
            if (
              state.kind === "consume" &&
              (typeof state.test === "string"
                ? state.test === char
                : state.test.test(char))
            ) {
              expand(state.next, next);
            }
          }
          if (next.length === 0) return false;
          [current, next] = [next, current];
        }
        return current.includes(0);
      } finally {
        chargeGlobWork(work);
      }
    },
  };
  while (
    cache.size >= MAX_CACHE_ENTRIES ||
    cacheWeight + matcher.weight > MAX_CACHE_WEIGHT
  ) {
    const oldest = cache.keys().next().value!;
    cacheWeight -= cache.get(oldest)!.weight;
    cache.delete(oldest);
  }
  cache.set(key, matcher);
  cacheWeight += matcher.weight;
  return matcher;
}

function findAlternatives(
  pattern: string,
): Map<number, { values: string[]; end: number }> {
  const result = new Map<number, { values: string[]; end: number }>();
  const stack: { start: number; commas: number[] }[] = [];
  for (let index = 0; index < pattern.length; index++) {
    const char = pattern[index];
    if (char === "{") stack.push({ start: index, commas: [] });
    else if (char === ",") stack.at(-1)?.commas.push(index);
    else if (char === "}") {
      const group = stack.pop();
      if (!group?.commas.length) continue;
      let from = group.start + 1;
      const values: string[] = [];
      for (const end of [...group.commas, index]) {
        values.push(pattern.slice(from, end));
        from = end + 1;
      }
      result.set(group.start, { values, end: index });
    }
  }
  return result;
}

function readCharacterClass(
  pattern: string,
  start: number,
  end: number,
  caseInsensitive: boolean,
): { test: RegExp; end: number } | undefined {
  if (end < 0) return undefined;
  let content = pattern.slice(start + 1, end);
  if (!content || content === "!" || content === "^") return undefined;
  const negated = content.startsWith("!") || content.startsWith("^");
  if (negated) content = content.slice(1);
  content = content.replaceAll("\\", "\\\\").replaceAll("/", "\\/");
  // Only a single character class, tested against one code unit. No quantifiers,
  // groups, or alternatives from the glob reach the JavaScript regex engine.
  return {
    test: new RegExp(
      `^[${negated ? "^" : ""}${content}]$`,
      caseInsensitive ? "i" : undefined,
    ),
    end,
  };
}

function escapeRegExp(value: string): string {
  return value.replace(/[|\\{}()[\]^$+*?.]/g, "\\$&");
}
