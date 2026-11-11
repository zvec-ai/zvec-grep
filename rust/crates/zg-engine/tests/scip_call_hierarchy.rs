//! Caller/callee checks derived from pinned SCIP indexer reference snapshots.
//!
//! Only actual source calls to project-local callable definitions are included.
//! Ordinary value/type references, external library calls and unproven targets
//! are excluded. Snapshot symbol identity supplies the target; source syntax
//! supplies the enclosing caller. Each checked call is asserted in both directions.
//! These are required-edge checks, not exhaustive call-graph snapshots.
//! Source text and expected values are inline; SCIP is not run by these tests.
//! Rust fixtures remove test cursor markers and add Cargo manifests for crate layout.
//! Upstream provenance is recorded beside each case; see LICENSE.scip.

#![recursion_limit = "512"]

#[allow(dead_code)]
#[path = "support/relationship_cases.rs"]
mod relationship_cases;
#[allow(dead_code)]
mod support;

use relationship_cases::{Case, Check, Query, SourceFile, check_case};
use serde_json::json;

// https://github.com/sourcegraph/scip-typescript/tree/cda58beb3bc49bdf043225a092821ca2d361fcb1/snapshots/input/syntax
// Snapshot: snapshots/output/syntax/src/class.ts:19
// Target: syntax 1.0.0 src/`class.ts`/Class#privateMethod().
// Snapshot: snapshots/output/syntax/src/class.ts:47
// Target: syntax 1.0.0 src/`class.ts`/Class#staticMethod().
#[tokio::test]
async fn scip_ts_private_and_static_methods() {
    check_case(Case {
        name: "scip_ts_private_and_static_methods",
        files: vec![
            SourceFile {
                path: "src/class.ts",
                source: "export class Class {\n  public classProperty: string\n  constructor(constructorParam: string) {\n    this.classProperty = constructorParam\n  }\n  public method(methodParam: string): string {\n    return this.privateMethod(methodParam)\n  }\n  public static staticMethod(methodParam: string): string {\n    return methodParam\n  }\n  private privateMethod(methodParam: string): string {\n    return methodParam\n  }\n}\n\nexport function newClass(param: string): string {\n  const instance = new Class(param).classProperty\n  const instance2 = Class.staticMethod(param)\n  return instance + instance2\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "method",
                expected: json!([
                    {"filePath": "src/class.ts", "startLine": 6, "edges": [
                        {"symbol": {"name": "privateMethod", "filePath": "src/class.ts", "startLine": 12}, "filePath": "src/class.ts", "line": 7},
                    ]},
                ]),
                file: Some("src/class.ts"),
                short_names: true,
                definition_line: Some(6),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "newClass",
                expected: json!([
                    {"filePath": "src/class.ts", "startLine": 17, "edges": [
                        {"symbol": {"name": "staticMethod", "filePath": "src/class.ts", "startLine": 9}, "filePath": "src/class.ts", "line": 19},
                    ]},
                ]),
                file: Some("src/class.ts"),
                short_names: true,
                definition_line: Some(17),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "privateMethod",
                expected: json!([
                    {"filePath": "src/class.ts", "startLine": 12, "edges": [
                        {"symbol": {"name": "method", "filePath": "src/class.ts", "startLine": 6}, "filePath": "src/class.ts", "line": 7},
                    ]},
                ]),
                file: Some("src/class.ts"),
                short_names: true,
                definition_line: Some(12),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "staticMethod",
                expected: json!([
                    {"filePath": "src/class.ts", "startLine": 9, "edges": [
                        {"symbol": {"name": "newClass", "filePath": "src/class.ts", "startLine": 17}, "filePath": "src/class.ts", "line": 19},
                    ]},
                ]),
                file: Some("src/class.ts"),
                short_names: true,
                definition_line: Some(9),
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/sourcegraph/scip-typescript/tree/cda58beb3bc49bdf043225a092821ca2d361fcb1/snapshots/input/syntax
// Snapshot: snapshots/output/syntax/src/object-literals-arrow-function.ts:35
// Target: syntax 1.0.0 src/`object-literals-arrow-function.ts`/hasArrowFunctionParameter().
// Snapshot: snapshots/output/syntax/src/object-literals-arrow-function.ts:43
// Target: syntax 1.0.0 src/`object-literals-arrow-function.ts`/hasArrowFunctionParameter().
#[tokio::test]
async fn scip_ts_arrow_arguments_and_repeated_calls() {
    check_case(Case {
        name: "scip_ts_arrow_arguments_and_repeated_calls",
        files: vec![
            SourceFile {
                path: "src/object-literals-arrow-function.ts",
                source: "import { Option } from './reusable-types'\n\ninterface Foobar {\n  foobar: number\n}\n\nexport function hasArrowFunctionParameter(\n  something: number,\n  fn: (foobar: Foobar) => Foobar\n): Foobar {\n  return fn({ foobar: 42 + something })\n}\n\nexport function consumesArrowFunction(): number {\n  return (\n    hasArrowFunctionParameter(1, ({ foobar }) => ({ foobar: foobar + 1 }))\n      .foobar +\n    hasArrowFunctionParameter(2, foobar => ({ foobar: foobar.foobar + 2 }))\n      .foobar\n  )\n}\n\nexport function genericArrow(): Foobar[] {\n  return [1].map<Foobar>(n => ({ foobar: n + 1 }))\n}\n\nexport function genericArrowOption(): Option<Foobar>[] {\n  return [1].map<Option<Foobar>>(n => ({ value: { foobar: n + 1 } }))\n}\n\nexport function genericArrow2(): Foobar[] {\n  // navigation to `foobar` below does not work with tsserver or scip-java\n  // because `map`  is missing an explicit `map<Foobar>` annotation.\n  return [1].map(n => ({ foobar: n + 1 }))\n}\n",
            },
            SourceFile {
                path: "src/reusable-types.ts",
                source: "// Reusable types for other snapshot tests\n\nexport interface Option<A> {\n  value?: A\n}\n\nexport interface Numbers {\n  property: number\n}\nexport interface Strings {\n  property2: string\n}\nexport type Configuration = Numbers & Strings\n\nexport class GenericClass<A> {\n  constructor(public readonly values: A[]) {}\n  public map(fn: (a: A) => A): A[] {\n    return this.values.map(a => fn(a))\n  }\n}\n\nexport interface Superinterface {\n  property: string\n  interfaceMethod(): string\n}\nexport interface GenericInterface<T> {\n  property: T\n  interfaceMethod(): string\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "consumesArrowFunction",
                expected: json!([
                    {"filePath": "src/object-literals-arrow-function.ts", "startLine": 14, "edges": [
                        {"symbol": {"name": "hasArrowFunctionParameter", "filePath": "src/object-literals-arrow-function.ts", "startLine": 7}, "filePath": "src/object-literals-arrow-function.ts", "line": 16},
                        {"symbol": {"name": "hasArrowFunctionParameter", "filePath": "src/object-literals-arrow-function.ts", "startLine": 7}, "filePath": "src/object-literals-arrow-function.ts", "line": 18},
                    ]},
                ]),
                file: Some("src/object-literals-arrow-function.ts"),
                short_names: true,
                definition_line: Some(14),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "hasArrowFunctionParameter",
                expected: json!([
                    {"filePath": "src/object-literals-arrow-function.ts", "startLine": 7, "edges": [
                        {"symbol": {"name": "consumesArrowFunction", "filePath": "src/object-literals-arrow-function.ts", "startLine": 14}, "filePath": "src/object-literals-arrow-function.ts", "line": 16},
                        {"symbol": {"name": "consumesArrowFunction", "filePath": "src/object-literals-arrow-function.ts", "startLine": 14}, "filePath": "src/object-literals-arrow-function.ts", "line": 18},
                    ]},
                ]),
                file: Some("src/object-literals-arrow-function.ts"),
                short_names: true,
                definition_line: Some(7),
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/sourcegraph/scip-typescript/tree/cda58beb3bc49bdf043225a092821ca2d361fcb1/snapshots/input/syntax
// Snapshot: snapshots/output/syntax/src/object-literals-call-signatures.ts:38
// Target: syntax 1.0.0 src/`object-literals-call-signatures.ts`/consumesInterface().
// Snapshot: snapshots/output/syntax/src/object-literals-call-signatures.ts:45
// Target: syntax 1.0.0 src/`object-literals-call-signatures.ts`/consumesArray().
// Snapshot: snapshots/output/syntax/src/object-literals-call-signatures.ts:54
// Target: syntax 1.0.0 src/`object-literals-call-signatures.ts`/consumesGenericInterface().
// Snapshot: snapshots/output/syntax/src/object-literals-call-signatures.ts:61
// Target: syntax 1.0.0 src/`object-literals-call-signatures.ts`/consumesGenericInterface().
// Snapshot: snapshots/output/syntax/src/object-literals-call-signatures.ts:131
// Target: syntax 1.0.0 src/`reusable-types.ts`/GenericClass#map().
// Snapshot: snapshots/output/syntax/src/object-literals-call-signatures.ts:130
// Target: syntax 1.0.0 src/`object-literals-call-signatures.ts`/createGenericClass().
// Snapshot: snapshots/output/syntax/src/object-literals-call-signatures.ts:152
// Target: syntax 1.0.0 src/`object-literals-call-signatures.ts`/consumesInterface().
#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "Keep the upstream source and its snapshot-derived queries together"
)]
async fn scip_ts_generic_calls_and_chained_receiver() {
    check_case(Case {
        name: "scip_ts_generic_calls_and_chained_receiver",
        files: vec![
            SourceFile {
                path: "src/object-literals-call-signatures.ts",
                source: "import {\n  Configuration,\n  GenericClass,\n  GenericInterface,\n  Option,\n  Superinterface,\n} from './reusable-types'\n\nexport function consumesInterface(superInterface: Superinterface): void {}\nexport function consumesArray(superInterface: Superinterface[]): void {}\nexport function consumesGenericInterface<T>(\n  genercInterface: GenericInterface<T>\n): void {}\n\nexport function infersInterface(): void {\n  consumesInterface({\n    interfaceMethod: (): string => 'inferred',\n    property: 'inferred',\n  })\n  consumesArray([\n    {\n      interfaceMethod: (): string => 'inferred',\n      property: 'inferred',\n    },\n  ])\n  consumesGenericInterface<number>({\n    interfaceMethod: (): string => 'inferred',\n    property: 123,\n  })\n  consumesGenericInterface<Option<Configuration>[]>({\n    interfaceMethod: (): string => 'inferred',\n    property: [{ value: { property: 42, property2: '42' } }],\n  })\n}\nexport function returnStatementInsideArgumentExpression(): Configuration[] {\n  if (1 == 1) {\n    return [1].map<Configuration>((number: number): Configuration => {\n      const incremented = number + 1\n      return {\n        property: incremented,\n        property2: incremented.toString(),\n      }\n    })\n  } else {\n    return [1].map<Configuration>(number => {\n      const incremented = number + 1\n      return {\n        property: incremented,\n        property2: incremented.toString(),\n      }\n    })\n  }\n}\n\nexport function createGenericClass(): GenericClass<Configuration> {\n  return new GenericClass<Configuration>([{ property: 1, property2: '2' }])\n}\n\nexport function handleGenericClass() {\n  return createGenericClass().map(({ property, property2 }) => ({\n    property: property + 1,\n    property2: property2 + '1',\n  }))\n}\n\nexport function handleShorthand() {\n  const property = '42'\n  const interfaceMethod = (): string => 'inferred'\n  consumesInterface({\n    interfaceMethod,\n    property,\n  })\n}\n",
            },
            SourceFile {
                path: "src/reusable-types.ts",
                source: "// Reusable types for other snapshot tests\n\nexport interface Option<A> {\n  value?: A\n}\n\nexport interface Numbers {\n  property: number\n}\nexport interface Strings {\n  property2: string\n}\nexport type Configuration = Numbers & Strings\n\nexport class GenericClass<A> {\n  constructor(public readonly values: A[]) {}\n  public map(fn: (a: A) => A): A[] {\n    return this.values.map(a => fn(a))\n  }\n}\n\nexport interface Superinterface {\n  property: string\n  interfaceMethod(): string\n}\nexport interface GenericInterface<T> {\n  property: T\n  interfaceMethod(): string\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "infersInterface",
                expected: json!([
                    {"filePath": "src/object-literals-call-signatures.ts", "startLine": 15, "edges": [
                        {"symbol": {"name": "consumesInterface", "filePath": "src/object-literals-call-signatures.ts", "startLine": 9}, "filePath": "src/object-literals-call-signatures.ts", "line": 16},
                        {"symbol": {"name": "consumesArray", "filePath": "src/object-literals-call-signatures.ts", "startLine": 10}, "filePath": "src/object-literals-call-signatures.ts", "line": 20},
                        {"symbol": {"name": "consumesGenericInterface", "filePath": "src/object-literals-call-signatures.ts", "startLine": 11}, "filePath": "src/object-literals-call-signatures.ts", "line": 26},
                        {"symbol": {"name": "consumesGenericInterface", "filePath": "src/object-literals-call-signatures.ts", "startLine": 11}, "filePath": "src/object-literals-call-signatures.ts", "line": 30},
                    ]},
                ]),
                file: Some("src/object-literals-call-signatures.ts"),
                short_names: true,
                definition_line: Some(15),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "handleGenericClass",
                expected: json!([
                    {"filePath": "src/object-literals-call-signatures.ts", "startLine": 59, "edges": [
                        {"symbol": {"name": "map", "filePath": "src/reusable-types.ts", "startLine": 17}, "filePath": "src/object-literals-call-signatures.ts", "line": 60},
                        {"symbol": {"name": "createGenericClass", "filePath": "src/object-literals-call-signatures.ts", "startLine": 55}, "filePath": "src/object-literals-call-signatures.ts", "line": 60},
                    ]},
                ]),
                file: Some("src/object-literals-call-signatures.ts"),
                short_names: true,
                definition_line: Some(59),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "handleShorthand",
                expected: json!([
                    {"filePath": "src/object-literals-call-signatures.ts", "startLine": 66, "edges": [
                        {"symbol": {"name": "consumesInterface", "filePath": "src/object-literals-call-signatures.ts", "startLine": 9}, "filePath": "src/object-literals-call-signatures.ts", "line": 69},
                    ]},
                ]),
                file: Some("src/object-literals-call-signatures.ts"),
                short_names: true,
                definition_line: Some(66),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "consumesInterface",
                expected: json!([
                    {"filePath": "src/object-literals-call-signatures.ts", "startLine": 9, "edges": [
                        {"symbol": {"name": "infersInterface", "filePath": "src/object-literals-call-signatures.ts", "startLine": 15}, "filePath": "src/object-literals-call-signatures.ts", "line": 16},
                        {"symbol": {"name": "handleShorthand", "filePath": "src/object-literals-call-signatures.ts", "startLine": 66}, "filePath": "src/object-literals-call-signatures.ts", "line": 69},
                    ]},
                ]),
                file: Some("src/object-literals-call-signatures.ts"),
                short_names: true,
                definition_line: Some(9),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "consumesArray",
                expected: json!([
                    {"filePath": "src/object-literals-call-signatures.ts", "startLine": 10, "edges": [
                        {"symbol": {"name": "infersInterface", "filePath": "src/object-literals-call-signatures.ts", "startLine": 15}, "filePath": "src/object-literals-call-signatures.ts", "line": 20},
                    ]},
                ]),
                file: Some("src/object-literals-call-signatures.ts"),
                short_names: true,
                definition_line: Some(10),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "consumesGenericInterface",
                expected: json!([
                    {"filePath": "src/object-literals-call-signatures.ts", "startLine": 11, "edges": [
                        {"symbol": {"name": "infersInterface", "filePath": "src/object-literals-call-signatures.ts", "startLine": 15}, "filePath": "src/object-literals-call-signatures.ts", "line": 26},
                        {"symbol": {"name": "infersInterface", "filePath": "src/object-literals-call-signatures.ts", "startLine": 15}, "filePath": "src/object-literals-call-signatures.ts", "line": 30},
                    ]},
                ]),
                file: Some("src/object-literals-call-signatures.ts"),
                short_names: true,
                definition_line: Some(11),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "map",
                expected: json!([
                    {"filePath": "src/reusable-types.ts", "startLine": 17, "edges": [
                        {"symbol": {"name": "handleGenericClass", "filePath": "src/object-literals-call-signatures.ts", "startLine": 59}, "filePath": "src/object-literals-call-signatures.ts", "line": 60},
                    ]},
                ]),
                file: Some("src/reusable-types.ts"),
                short_names: true,
                definition_line: Some(17),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "createGenericClass",
                expected: json!([
                    {"filePath": "src/object-literals-call-signatures.ts", "startLine": 55, "edges": [
                        {"symbol": {"name": "handleGenericClass", "filePath": "src/object-literals-call-signatures.ts", "startLine": 59}, "filePath": "src/object-literals-call-signatures.ts", "line": 60},
                    ]},
                ]),
                file: Some("src/object-literals-call-signatures.ts"),
                short_names: true,
                definition_line: Some(55),
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/sourcegraph/scip-typescript/tree/cda58beb3bc49bdf043225a092821ca2d361fcb1/snapshots/input/pure-js
// Snapshot: snapshots/output/pure-js/src/main.js:12
// Target: pure-js 1.0.0 src/`main.js`/fib().
// Snapshot: snapshots/output/pure-js/src/main.js:14
// Target: pure-js 1.0.0 src/`main.js`/fib().
// Snapshot: snapshots/output/pure-js/src/main.js:24
// Target: pure-js 1.0.0 src/`main.js`/fib().
// Snapshot: snapshots/output/pure-js/src/main.js:61
// Target: pure-js 1.0.0 src/`main.js`/forever().
// Snapshot: snapshots/output/pure-js/src/main.js:67
// Target: pure-js 1.0.0 src/`main.js`/print_fib().
// Snapshot: snapshots/output/pure-js/src/main.js:73
// Target: pure-js 1.0.0 src/`main.js`/forever().
// Snapshot: snapshots/output/pure-js/src/main.js:78
// Target: pure-js 1.0.0 src/`main.js`/print_fib().
// Snapshot: snapshots/output/pure-js/src/main.js:87
// Target: pure-js 1.0.0 src/`main.js`/forever().
// Snapshot: snapshots/output/pure-js/src/main.js:92
// Target: pure-js 1.0.0 src/`main.js`/print_fib().
#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "Keep the upstream source and its snapshot-derived queries together"
)]
async fn scip_js_recursion_and_forward_calls() {
    check_case(Case {
        name: "scip_js_recursion_and_forward_calls",
        files: vec![
            SourceFile {
                path: "src/main.js",
                source: "function fib(n) {\n  if (n <= 1) {\n    return 0\n  }\n  return fib(n - 1) + fib(n - 2)\n}\n\nfunction print_fib(a) {\n  console.log(fib(a))\n}\n\nvar y = 'Hello'\nfunction capture() {\n  return y\n}\nconst capture_lambda = () => {\n  return y\n}\n\nfor (var i = 0; i <= 10; i++) {}\n\nfor (const x of [1, 2, 3]) {\n}\n\nvar a = 0\nvar a = 1\nprint_fib(a)\n\nfunction forever() {\n  return forever()\n}\n\nfunction use_before_def() {\n  print_fib(n)\n  var n = 10\n\n  if (forever()) {\n    var m = 10\n  }\n  print_fib(m)\n}\n\nfunction var_function_scope() {\n  var k = 0\n  if (forever()) {\n    var k = 1\n  }\n  print_fib(k)\n}\n\nfunction array_of_objects() {\n  var a = [{ element: 0 }, { element: 1 }]\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "fib",
                expected: json!([
                    {"filePath": "src/main.js", "startLine": 1, "edges": [
                        {"symbol": {"name": "fib", "filePath": "src/main.js", "startLine": 1}, "filePath": "src/main.js", "line": 5},
                        {"symbol": {"name": "fib", "filePath": "src/main.js", "startLine": 1}, "filePath": "src/main.js", "line": 5},
                    ]},
                ]),
                file: Some("src/main.js"),
                short_names: true,
                definition_line: Some(1),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "print_fib",
                expected: json!([
                    {"filePath": "src/main.js", "startLine": 8, "edges": [
                        {"symbol": {"name": "fib", "filePath": "src/main.js", "startLine": 1}, "filePath": "src/main.js", "line": 9},
                    ]},
                ]),
                file: Some("src/main.js"),
                short_names: true,
                definition_line: Some(8),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "forever",
                expected: json!([
                    {"filePath": "src/main.js", "startLine": 29, "edges": [
                        {"symbol": {"name": "forever", "filePath": "src/main.js", "startLine": 29}, "filePath": "src/main.js", "line": 30},
                    ]},
                ]),
                file: Some("src/main.js"),
                short_names: true,
                definition_line: Some(29),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "use_before_def",
                expected: json!([
                    {"filePath": "src/main.js", "startLine": 33, "edges": [
                        {"symbol": {"name": "print_fib", "filePath": "src/main.js", "startLine": 8}, "filePath": "src/main.js", "line": 34},
                        {"symbol": {"name": "forever", "filePath": "src/main.js", "startLine": 29}, "filePath": "src/main.js", "line": 37},
                        {"symbol": {"name": "print_fib", "filePath": "src/main.js", "startLine": 8}, "filePath": "src/main.js", "line": 40},
                    ]},
                ]),
                file: Some("src/main.js"),
                short_names: true,
                definition_line: Some(33),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "var_function_scope",
                expected: json!([
                    {"filePath": "src/main.js", "startLine": 43, "edges": [
                        {"symbol": {"name": "forever", "filePath": "src/main.js", "startLine": 29}, "filePath": "src/main.js", "line": 45},
                        {"symbol": {"name": "print_fib", "filePath": "src/main.js", "startLine": 8}, "filePath": "src/main.js", "line": 48},
                    ]},
                ]),
                file: Some("src/main.js"),
                short_names: true,
                definition_line: Some(43),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "fib",
                expected: json!([
                    {"filePath": "src/main.js", "startLine": 1, "edges": [
                        {"symbol": {"name": "fib", "filePath": "src/main.js", "startLine": 1}, "filePath": "src/main.js", "line": 5},
                        {"symbol": {"name": "fib", "filePath": "src/main.js", "startLine": 1}, "filePath": "src/main.js", "line": 5},
                        {"symbol": {"name": "print_fib", "filePath": "src/main.js", "startLine": 8}, "filePath": "src/main.js", "line": 9},
                    ]},
                ]),
                file: Some("src/main.js"),
                short_names: true,
                definition_line: Some(1),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "forever",
                expected: json!([
                    {"filePath": "src/main.js", "startLine": 29, "edges": [
                        {"symbol": {"name": "forever", "filePath": "src/main.js", "startLine": 29}, "filePath": "src/main.js", "line": 30},
                        {"symbol": {"name": "use_before_def", "filePath": "src/main.js", "startLine": 33}, "filePath": "src/main.js", "line": 37},
                        {"symbol": {"name": "var_function_scope", "filePath": "src/main.js", "startLine": 43}, "filePath": "src/main.js", "line": 45},
                    ]},
                ]),
                file: Some("src/main.js"),
                short_names: true,
                definition_line: Some(29),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "print_fib",
                expected: json!([
                    {"filePath": "src/main.js", "startLine": 8, "edges": [
                        {"symbol": {"name": "use_before_def", "filePath": "src/main.js", "startLine": 33}, "filePath": "src/main.js", "line": 34},
                        {"symbol": {"name": "use_before_def", "filePath": "src/main.js", "startLine": 33}, "filePath": "src/main.js", "line": 40},
                        {"symbol": {"name": "var_function_scope", "filePath": "src/main.js", "startLine": 43}, "filePath": "src/main.js", "line": 48},
                    ]},
                ]),
                file: Some("src/main.js"),
                short_names: true,
                definition_line: Some(8),
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/sourcegraph/scip-python/tree/4bf0b82356b43151c0d41b7cb3444eff88877864/packages/pyright-scip/snapshots/input/graph_1278
// Snapshot: packages/pyright-scip/snapshots/output/graph_1278/mwe.py:61
// Target:  snapshot-util 0.1 mwe/noop().
#[tokio::test]
async fn scip_python_graph_1278() {
    check_case(Case {
        name: "scip_python_graph_1278",
        files: vec![
            SourceFile {
                path: "mwe.py",
                source: "from typing import TypeVar, Generic, Callable, Iterator, ParamSpec\n\n_T_co = TypeVar(\"_T_co\")\n_P = ParamSpec(\"_P\")\n\nclass X(Generic[_T_co]):\n    pass\n\ndef decorate(func: Callable[_P, Iterator[_T_co]]) -> Callable[_P, X[_T_co]]: ...\n\nclass Foo:\n    @decorate\n    def foo(self) -> Iterator[None]: ...\n\n@decorate\ndef noop():\n    yield\n\nclass FooImpl(Foo):\n    def foo(self):\n        return noop()",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "foo",
                expected: json!([
                    {"filePath": "mwe.py", "startLine": 20, "edges": [
                        {"symbol": {"name": "noop", "filePath": "mwe.py", "startLine": 16}, "filePath": "mwe.py", "line": 21},
                    ]},
                ]),
                file: Some("mwe.py"),
                short_names: true,
                definition_line: Some(20),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "noop",
                expected: json!([
                    {"filePath": "mwe.py", "startLine": 16, "edges": [
                        {"symbol": {"name": "foo", "filePath": "mwe.py", "startLine": 20}, "filePath": "mwe.py", "line": 21},
                    ]},
                ]),
                file: Some("mwe.py"),
                short_names: true,
                definition_line: Some(16),
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/sourcegraph/scip-python/tree/4bf0b82356b43151c0d41b7cb3444eff88877864/packages/pyright-scip/snapshots/input/pyproject_1
// Snapshot: packages/pyright-scip/snapshots/output/pyproject_1/main.py:10
// Target:  abc 16.05 helpers/value().
#[tokio::test]
async fn scip_python_pyproject_1() {
    check_case(Case {
        name: "scip_python_pyproject_1",
        files: vec![
            SourceFile {
                path: "main.py",
                source: "from helpers import value\n\ndef main():\n    return value()\n\nmain()\n",
            },
            SourceFile {
                path: "lib/helpers.py",
                source: "def value():\n    return 42\n",
            },
            SourceFile {
                path: "pyproject.toml",
                source: "[project]\nname = \"abc\"\n\n[tool.poetry]\nversion = \"16.05\"\n\n[dependency-groups]\ndev = [{include-group = \"test\"}, \"requests\"]\n\n[tool.example]\nmax_id = 9223372036854775807\n\n[tool.pyright]\nexclude = [\"main.py\"]\n\n[tool.scip]\ninclude = [\"main.py\"]\nextraPaths = [\"lib\"]\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "main",
                expected: json!([
                    {"filePath": "main.py", "startLine": 3, "edges": [
                        {"symbol": {"name": "value", "filePath": "lib/helpers.py", "startLine": 1}, "filePath": "main.py", "line": 4},
                    ]},
                ]),
                file: Some("main.py"),
                short_names: true,
                definition_line: Some(3),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "value",
                expected: json!([
                    {"filePath": "lib/helpers.py", "startLine": 1, "edges": [
                        {"symbol": {"name": "main", "filePath": "main.py", "startLine": 3}, "filePath": "main.py", "line": 4},
                    ]},
                ]),
                file: Some("lib/helpers.py"),
                short_names: true,
                definition_line: Some(1),
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/sourcegraph/scip-python/tree/4bf0b82356b43151c0d41b7cb3444eff88877864/packages/pyright-scip/snapshots/input/issue_223
// Snapshot: packages/pyright-scip/snapshots/output/issue_223/contracts.py:169
// Target:  scip-final-repro 0.1.0 contracts/_validate().
#[tokio::test]
async fn scip_python_issue_223() {
    check_case(Case {
        name: "scip_python_issue_223",
        files: vec![
            SourceFile {
                path: "contracts.py",
                source: "# format-options: showDocs,showRanges\n\nfrom __future__ import annotations\n\nfrom dataclasses import dataclass\nfrom numbers import Real\nimport math\n\n\ndef _validate(value: object) -> None:\n    if not isinstance(value, Foo):\n        raise ValueError(\"must be Foo\")\n    if not isinstance(value.x, Real) or not math.isfinite(value.x):\n        raise ValueError(\"x must be finite real\")\n\n\n@dataclass(frozen=True)\nclass Foo:\n    x: float\n\n    def __post_init__(self) -> None:\n        _validate(self)\n",
            },
            SourceFile {
                path: "consumer.py",
                source: "from __future__ import annotations\n\nfrom contracts import Foo\n\n\ndef make_foo(x: float) -> Foo:\n    return Foo(x=x)\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "__post_init__",
                expected: json!([
                    {"filePath": "contracts.py", "startLine": 21, "edges": [
                        {"symbol": {"name": "_validate", "filePath": "contracts.py", "startLine": 10}, "filePath": "contracts.py", "line": 22},
                    ]},
                ]),
                file: Some("contracts.py"),
                short_names: true,
                definition_line: Some(21),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "_validate",
                expected: json!([
                    {"filePath": "contracts.py", "startLine": 10, "edges": [
                        {"symbol": {"name": "__post_init__", "filePath": "contracts.py", "startLine": 21}, "filePath": "contracts.py", "line": 22},
                    ]},
                ]),
                file: Some("contracts.py"),
                short_names: true,
                definition_line: Some(10),
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/sourcegraph/scip-go/tree/79ba4dda5ba9addb4adbec8f56adf138345b5515/internal/testdata/snapshots/input/initial
// Snapshot: internal/testdata/snapshots/output/initial/func_declarations.go:12
// Target: 0.1.test `sg/initial`/DefinedLater().
#[tokio::test]
async fn scip_go_initial() {
    check_case(Case {
        name: "scip_go_initial",
        files: vec![
            SourceFile {
                path: "func_declarations.go",
                source: "package initial\n\nfunc UsesLater() {\n\tDefinedLater()\n}\n\nfunc DefinedLater() {}\n",
            },
            SourceFile {
                path: "go.mod",
                source: "module sg/initial\n\ngo 1.23\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "UsesLater",
                expected: json!([
                    {"filePath": "func_declarations.go", "startLine": 3, "edges": [
                        {"symbol": {"name": "DefinedLater", "filePath": "func_declarations.go", "startLine": 7}, "filePath": "func_declarations.go", "line": 4},
                    ]},
                ]),
                file: Some("func_declarations.go"),
                short_names: true,
                definition_line: Some(3),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "DefinedLater",
                expected: json!([
                    {"filePath": "func_declarations.go", "startLine": 7, "edges": [
                        {"symbol": {"name": "UsesLater", "filePath": "func_declarations.go", "startLine": 3}, "filePath": "func_declarations.go", "line": 4},
                    ]},
                ]),
                file: Some("func_declarations.go"),
                short_names: true,
                definition_line: Some(7),
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/sourcegraph/scip-go/tree/79ba4dda5ba9addb4adbec8f56adf138345b5515/internal/testdata/snapshots/input/generallyeric
// Snapshot: internal/testdata/snapshots/output/generallyeric/person.go:84
// Target: 0.1.test `sg/generallyeric`/Person#Work.
// Snapshot: internal/testdata/snapshots/output/generallyeric/person.go:120
// Target: 0.1.test `sg/generallyeric`/DoWork().
#[tokio::test]
async fn scip_go_generallyeric() {
    check_case(Case {
        name: "scip_go_generallyeric",
        files: vec![
            SourceFile {
                path: "person.go",
                source: "package generallyeric\n\nimport \"fmt\"\n\ntype Person interface {\n\tWork()\n}\n\ntype worker string\n\nfunc (w worker) Work() {\n\tfmt.Printf(\"%s is working\\n\", w)\n}\n\nfunc DoWork[T Person](things []T) {\n\tfor _, v := range things {\n\t\tv.Work()\n\t}\n}\n\nfunc main() {\n\tvar a, b, c worker\n\ta = \"A\"\n\tb = \"B\"\n\tc = \"C\"\n\tDoWork([]worker{a, b, c})\n}\n",
            },
            SourceFile {
                path: "go.mod",
                source: "module sg/generallyeric\n\ngo 1.19\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "DoWork",
                expected: json!([
                    {"filePath": "person.go", "startLine": 15, "edges": [
                        {"symbol": {"name": "Work", "filePath": "person.go", "startLine": 6}, "filePath": "person.go", "line": 17},
                    ]},
                ]),
                file: Some("person.go"),
                short_names: true,
                definition_line: Some(15),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "main",
                expected: json!([
                    {"filePath": "person.go", "startLine": 21, "edges": [
                        {"symbol": {"name": "DoWork", "filePath": "person.go", "startLine": 15}, "filePath": "person.go", "line": 26},
                    ]},
                ]),
                file: Some("person.go"),
                short_names: true,
                definition_line: Some(21),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "Work",
                expected: json!([
                    {"filePath": "person.go", "startLine": 6, "edges": [
                        {"symbol": {"name": "DoWork", "filePath": "person.go", "startLine": 15}, "filePath": "person.go", "line": 17},
                    ]},
                ]),
                file: Some("person.go"),
                short_names: true,
                definition_line: Some(6),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "DoWork",
                expected: json!([
                    {"filePath": "person.go", "startLine": 15, "edges": [
                        {"symbol": {"name": "main", "filePath": "person.go", "startLine": 21}, "filePath": "person.go", "line": 26},
                    ]},
                ]),
                file: Some("person.go"),
                short_names: true,
                definition_line: Some(15),
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/sourcegraph/scip-go/tree/79ba4dda5ba9addb4adbec8f56adf138345b5515/internal/testdata/snapshots/input/switches
// Snapshot: internal/testdata/snapshots/output/switches/switches.go:65
// Target: 0.1.test `sg/switches`/CustomSwitch#Something().
#[tokio::test]
async fn scip_go_switches() {
    check_case(Case {
        name: "scip_go_switches",
        files: vec![
            SourceFile {
                path: "switches.go",
                source: "package switches\n\n// CustomSwitch does the things in a switch\ntype CustomSwitch struct{}\n\n// Something does some things... and stuff\nfunc (c *CustomSwitch) Something() bool { return false }\n\nfunc Switch(interfaceValue interface{}) bool {\n\tswitch concreteValue := interfaceValue.(type) {\n\tcase int:\n\t\treturn concreteValue*3 > 10\n\tcase bool:\n\t\treturn !concreteValue\n\tcase CustomSwitch:\n\t\treturn concreteValue.Something()\n\tdefault:\n\t\treturn false\n\t}\n}\n",
            },
            SourceFile {
                path: "go.mod",
                source: "module sg/switches\n\ngo 1.19\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "Switch",
                expected: json!([
                    {"filePath": "switches.go", "startLine": 9, "edges": [
                        {"symbol": {"name": "Something", "filePath": "switches.go", "startLine": 7}, "filePath": "switches.go", "line": 16},
                    ]},
                ]),
                file: Some("switches.go"),
                short_names: true,
                definition_line: Some(9),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "Something",
                expected: json!([
                    {"filePath": "switches.go", "startLine": 7, "edges": [
                        {"symbol": {"name": "Switch", "filePath": "switches.go", "startLine": 9}, "filePath": "switches.go", "line": 16},
                    ]},
                ]),
                file: Some("switches.go"),
                short_names: true,
                definition_line: Some(7),
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/sourcegraph/scip-java/tree/3793579c53edb5c0a51c69315c07faca7376dc3e/scip-snapshots/cases/java/common
// Snapshot: scip-snapshots/expected/java/common/scip-snapshots/cases/java/common/src/main/java/minimized/Methods.java:131
// Target: scip-java maven . . minimized/Methods#staticOverload().
// Snapshot: scip-snapshots/expected/java/common/scip-snapshots/cases/java/common/src/main/java/minimized/Methods.java:142
// Target: scip-java maven . . minimized/Methods#staticOverload(+1).
// Snapshot: scip-snapshots/expected/java/common/scip-snapshots/cases/java/common/src/main/java/minimized/Methods.java:153
// Target: scip-java maven . . minimized/Methods#overload().
// Snapshot: scip-snapshots/expected/java/common/scip-snapshots/cases/java/common/src/main/java/minimized/Methods.java:165
// Target: scip-java maven . . minimized/Methods#overload(+1).
#[tokio::test]
async fn scip_java_overload_targets() {
    check_case(Case {
        name: "scip_java_overload_targets",
        files: vec![
            SourceFile {
                path: "src/main/java/minimized/Methods.java",
                source: "package minimized;\n\npublic class Methods {\n  private int overload(int value) {\n    return value + 1;\n  }\n\n  private String overload(String value) {\n    return value + \"1\";\n  }\n\n  private static int staticOverload(int value) {\n    return value + 1;\n  }\n\n  private static String staticOverload(String value) {\n    return value + \"1\";\n  }\n\n  public static String app(int n, String m) throws RuntimeException, IndexOutOfBoundsException {\n    Methods methods = new Methods();\n    int a = staticOverload(n);\n    String b = staticOverload(m);\n    int c = methods.overload(n);\n    String d = methods.overload(m);\n    return b + a + c + d;\n  }\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "app",
                expected: json!([
                    {"filePath": "src/main/java/minimized/Methods.java", "startLine": 20, "edges": [
                        {"symbol": {"name": "staticOverload", "filePath": "src/main/java/minimized/Methods.java", "startLine": 12}, "filePath": "src/main/java/minimized/Methods.java", "line": 22},
                        {"symbol": {"name": "staticOverload", "filePath": "src/main/java/minimized/Methods.java", "startLine": 16}, "filePath": "src/main/java/minimized/Methods.java", "line": 23},
                        {"symbol": {"name": "overload", "filePath": "src/main/java/minimized/Methods.java", "startLine": 4}, "filePath": "src/main/java/minimized/Methods.java", "line": 24},
                        {"symbol": {"name": "overload", "filePath": "src/main/java/minimized/Methods.java", "startLine": 8}, "filePath": "src/main/java/minimized/Methods.java", "line": 25},
                    ]},
                ]),
                file: Some("src/main/java/minimized/Methods.java"),
                short_names: true,
                definition_line: Some(20),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "staticOverload",
                expected: json!([
                    {"filePath": "src/main/java/minimized/Methods.java", "startLine": 12, "edges": [
                        {"symbol": {"name": "app", "filePath": "src/main/java/minimized/Methods.java", "startLine": 20}, "filePath": "src/main/java/minimized/Methods.java", "line": 22},
                    ]},
                ]),
                file: Some("src/main/java/minimized/Methods.java"),
                short_names: true,
                definition_line: Some(12),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "staticOverload",
                expected: json!([
                    {"filePath": "src/main/java/minimized/Methods.java", "startLine": 16, "edges": [
                        {"symbol": {"name": "app", "filePath": "src/main/java/minimized/Methods.java", "startLine": 20}, "filePath": "src/main/java/minimized/Methods.java", "line": 23},
                    ]},
                ]),
                file: Some("src/main/java/minimized/Methods.java"),
                short_names: true,
                definition_line: Some(16),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "overload",
                expected: json!([
                    {"filePath": "src/main/java/minimized/Methods.java", "startLine": 4, "edges": [
                        {"symbol": {"name": "app", "filePath": "src/main/java/minimized/Methods.java", "startLine": 20}, "filePath": "src/main/java/minimized/Methods.java", "line": 24},
                    ]},
                ]),
                file: Some("src/main/java/minimized/Methods.java"),
                short_names: true,
                definition_line: Some(4),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "overload",
                expected: json!([
                    {"filePath": "src/main/java/minimized/Methods.java", "startLine": 8, "edges": [
                        {"symbol": {"name": "app", "filePath": "src/main/java/minimized/Methods.java", "startLine": 20}, "filePath": "src/main/java/minimized/Methods.java", "line": 25},
                    ]},
                ]),
                file: Some("src/main/java/minimized/Methods.java"),
                short_names: true,
                definition_line: Some(8),
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/sourcegraph/scip-java/tree/3793579c53edb5c0a51c69315c07faca7376dc3e/scip-snapshots/cases/java/common
// Snapshot: scip-snapshots/expected/java/common/scip-snapshots/cases/java/common/src/main/java/minimized/Interfaces.java:88
// Target: scip-java maven . . minimized/BookService#checkPages().
// Snapshot: scip-snapshots/expected/java/common/scip-snapshots/cases/java/common/src/main/java/minimized/Interfaces.java:87
// Target: scip-java maven . . minimized/MyService#bookService().
#[tokio::test]
async fn scip_java_chained_interface_receiver() {
    check_case(Case {
        name: "scip_java_chained_interface_receiver",
        files: vec![
            SourceFile {
                path: "src/main/java/minimized/Interfaces.java",
                source: "package minimized;\n\npublic interface Interfaces {\n  static void staticInterfaceMethod() {}\n\n  String abstractInterfaceMethod();\n\n  default String defaultInterfaceMethod() {\n    return \"default\";\n  }\n}\n\ninterface BookService {\n  void checkPages();\n}\n\ninterface MyService {\n  BookService bookService();\n\n  default void example() {\n    bookService().checkPages();\n  }\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "example",
                expected: json!([
                    {"filePath": "src/main/java/minimized/Interfaces.java", "startLine": 20, "edges": [
                        {"symbol": {"name": "checkPages", "filePath": "src/main/java/minimized/Interfaces.java", "startLine": 14}, "filePath": "src/main/java/minimized/Interfaces.java", "line": 21},
                        {"symbol": {"name": "bookService", "filePath": "src/main/java/minimized/Interfaces.java", "startLine": 18}, "filePath": "src/main/java/minimized/Interfaces.java", "line": 21},
                    ]},
                ]),
                file: Some("src/main/java/minimized/Interfaces.java"),
                short_names: true,
                definition_line: Some(20),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "checkPages",
                expected: json!([
                    {"filePath": "src/main/java/minimized/Interfaces.java", "startLine": 14, "edges": [
                        {"symbol": {"name": "example", "filePath": "src/main/java/minimized/Interfaces.java", "startLine": 20}, "filePath": "src/main/java/minimized/Interfaces.java", "line": 21},
                    ]},
                ]),
                file: Some("src/main/java/minimized/Interfaces.java"),
                short_names: true,
                definition_line: Some(14),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "bookService",
                expected: json!([
                    {"filePath": "src/main/java/minimized/Interfaces.java", "startLine": 18, "edges": [
                        {"symbol": {"name": "example", "filePath": "src/main/java/minimized/Interfaces.java", "startLine": 20}, "filePath": "src/main/java/minimized/Interfaces.java", "line": 21},
                    ]},
                ]),
                file: Some("src/main/java/minimized/Interfaces.java"),
                short_names: true,
                definition_line: Some(18),
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/sourcegraph/scip-java/tree/3793579c53edb5c0a51c69315c07faca7376dc3e/scip-snapshots/cases/java/common
// Snapshot: scip-snapshots/expected/java/common/scip-snapshots/cases/java/common/src/main/java/minimized/SubClasses.java:72
// Target: scip-java maven . . minimized/SubClasses#abstractImplementation().
// Snapshot: scip-snapshots/expected/java/common/scip-snapshots/cases/java/common/src/main/java/minimized/SubClasses.java:75
// Target: scip-java maven . . minimized/AbstractClasses#defaultImplementation().
// Snapshot: scip-snapshots/expected/java/common/scip-snapshots/cases/java/common/src/main/java/minimized/SubClasses.java:78
// Target: scip-java maven . . minimized/SubClasses#abstractInterfaceMethod().
// Snapshot: scip-snapshots/expected/java/common/scip-snapshots/cases/java/common/src/main/java/minimized/SubClasses.java:81
// Target: scip-java maven . . minimized/Interfaces#defaultInterfaceMethod().
#[tokio::test]
async fn scip_java_inherited_and_default_methods() {
    check_case(Case {
        name: "scip_java_inherited_and_default_methods",
        files: vec![
            SourceFile {
                path: "src/main/java/minimized/SubClasses.java",
                source: "package minimized;\n\npublic class SubClasses extends AbstractClasses implements Interfaces {\n\n  @Override\n  public String abstractImplementation() {\n    return \"abstract\";\n  }\n\n  @Override\n  public String abstractInterfaceMethod() {\n    return \"abstractInterface\";\n  }\n\n  public static String app() {\n    SubClasses s = new SubClasses();\n    return s.abstractImplementation()\n        + s.defaultImplementation()\n        + s.abstractInterfaceMethod()\n        + s.defaultInterfaceMethod();\n  }\n}\n",
            },
            SourceFile {
                path: "src/main/java/minimized/AbstractClasses.java",
                source: "package minimized;\n\npublic abstract class AbstractClasses {\n  public String defaultImplementation() {\n    return \"\";\n  }\n\n  public abstract String abstractImplementation();\n}\n",
            },
            SourceFile {
                path: "src/main/java/minimized/Interfaces.java",
                source: "package minimized;\n\npublic interface Interfaces {\n  static void staticInterfaceMethod() {}\n\n  String abstractInterfaceMethod();\n\n  default String defaultInterfaceMethod() {\n    return \"default\";\n  }\n}\n\ninterface BookService {\n  void checkPages();\n}\n\ninterface MyService {\n  BookService bookService();\n\n  default void example() {\n    bookService().checkPages();\n  }\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "app",
                expected: json!([
                    {"filePath": "src/main/java/minimized/SubClasses.java", "startLine": 15, "edges": [
                        {"symbol": {"name": "abstractImplementation", "filePath": "src/main/java/minimized/SubClasses.java", "startLine": 5}, "filePath": "src/main/java/minimized/SubClasses.java", "line": 17},
                        {"symbol": {"name": "defaultImplementation", "filePath": "src/main/java/minimized/AbstractClasses.java", "startLine": 4}, "filePath": "src/main/java/minimized/SubClasses.java", "line": 18},
                        {"symbol": {"name": "abstractInterfaceMethod", "filePath": "src/main/java/minimized/SubClasses.java", "startLine": 10}, "filePath": "src/main/java/minimized/SubClasses.java", "line": 19},
                        {"symbol": {"name": "defaultInterfaceMethod", "filePath": "src/main/java/minimized/Interfaces.java", "startLine": 8}, "filePath": "src/main/java/minimized/SubClasses.java", "line": 20},
                    ]},
                ]),
                file: Some("src/main/java/minimized/SubClasses.java"),
                short_names: true,
                definition_line: Some(15),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "abstractImplementation",
                expected: json!([
                    {"filePath": "src/main/java/minimized/SubClasses.java", "startLine": 5, "edges": [
                        {"symbol": {"name": "app", "filePath": "src/main/java/minimized/SubClasses.java", "startLine": 15}, "filePath": "src/main/java/minimized/SubClasses.java", "line": 17},
                    ]},
                ]),
                file: Some("src/main/java/minimized/SubClasses.java"),
                short_names: true,
                definition_line: Some(5),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "defaultImplementation",
                expected: json!([
                    {"filePath": "src/main/java/minimized/AbstractClasses.java", "startLine": 4, "edges": [
                        {"symbol": {"name": "app", "filePath": "src/main/java/minimized/SubClasses.java", "startLine": 15}, "filePath": "src/main/java/minimized/SubClasses.java", "line": 18},
                    ]},
                ]),
                file: Some("src/main/java/minimized/AbstractClasses.java"),
                short_names: true,
                definition_line: Some(4),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "abstractInterfaceMethod",
                expected: json!([
                    {"filePath": "src/main/java/minimized/SubClasses.java", "startLine": 10, "edges": [
                        {"symbol": {"name": "app", "filePath": "src/main/java/minimized/SubClasses.java", "startLine": 15}, "filePath": "src/main/java/minimized/SubClasses.java", "line": 19},
                    ]},
                ]),
                file: Some("src/main/java/minimized/SubClasses.java"),
                short_names: true,
                definition_line: Some(10),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "defaultInterfaceMethod",
                expected: json!([
                    {"filePath": "src/main/java/minimized/Interfaces.java", "startLine": 8, "edges": [
                        {"symbol": {"name": "app", "filePath": "src/main/java/minimized/SubClasses.java", "startLine": 15}, "filePath": "src/main/java/minimized/SubClasses.java", "line": 20},
                    ]},
                ]),
                file: Some("src/main/java/minimized/Interfaces.java"),
                short_names: true,
                definition_line: Some(8),
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/sourcegraph/scip-java/tree/3793579c53edb5c0a51c69315c07faca7376dc3e/scip-snapshots/cases/java/common
// Snapshot: scip-snapshots/expected/java/common/scip-snapshots/cases/java/common/src/main/java/minimized/InnerClasses.java:322
// Target: scip-java maven . . minimized/InnerClasses#InnerInterface#apply().
// Snapshot: scip-snapshots/expected/java/common/scip-snapshots/cases/java/common/src/main/java/minimized/InnerClasses.java:444
// Target: scip-java maven . . minimized/InnerClasses#InnerInterface#apply().
// Snapshot: scip-snapshots/expected/java/common/scip-snapshots/cases/java/common/src/main/java/minimized/InnerClasses.java:466
// Target: scip-java maven . . minimized/InnerClasses#InnerStaticClass#innerStaticMethod().
// Snapshot: scip-snapshots/expected/java/common/scip-snapshots/cases/java/common/src/main/java/minimized/InnerClasses.java:492
// Target: scip-java maven . . minimized/InnerClasses#InnerClass#innerMethod().
// Snapshot: scip-snapshots/expected/java/common/scip-snapshots/cases/java/common/src/main/java/minimized/InnerClasses.java:497
// Target: scip-java maven . . minimized/InnerClasses#runInnerInterface().
// Snapshot: scip-snapshots/expected/java/common/scip-snapshots/cases/java/common/src/main/java/minimized/InnerClasses.java:501
// Target: scip-java maven . . minimized/InnerClasses#testEnum().
// Snapshot: scip-snapshots/expected/java/common/scip-snapshots/cases/java/common/src/main/java/minimized/InnerClasses.java:505
// Target: scip-java maven . . minimized/InnerClasses#testAnon().
#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "Keep the upstream source and its snapshot-derived queries together"
)]
async fn scip_java_inner_classes() {
    check_case(Case {
        name: "scip_java_inner_classes",
        files: vec![
            SourceFile {
                path: "src/main/java/minimized/InnerClasses.java",
                source: "package minimized;\n\npublic class InnerClasses {\n\n  private final int exampleField;\n\n  private static final String STRING = \"asdf\";\n\n  private static final int top = 5;\n  private static final int bottom = 10;\n\n  public InnerClasses(int exampleField) {\n    this.exampleField = exampleField;\n  }\n\n  public enum InnerEnum {\n    A,\n    B,\n    C\n  }\n\n  public interface InnerInterface<A, B> {\n    B apply(A a);\n  }\n\n  public @interface InnerAnnotation {\n    int value();\n  }\n\n  @SuppressWarnings(STRING + \" \")\n  @InnerAnnotation(top / bottom)\n  public static class InnerStaticClass {\n\n    public static void innerStaticMethod() {}\n  }\n\n  public class InnerClass implements InnerInterface<Integer, Integer> {\n    private final int field;\n\n    public InnerClass(int field) {\n      this.field = field;\n    }\n\n    public void innerMethod() {\n      System.out.println(field + exampleField);\n    }\n\n    @Override\n    public Integer apply(Integer integer) {\n      return field * integer;\n    }\n  }\n\n  private static <A, B> B runInnerInterface(InnerInterface<A, B> fn, A a) {\n    return fn.apply(a);\n  }\n\n  public static void testEnum(InnerEnum magicEnum) {\n    if (System.nanoTime() > System.currentTimeMillis()) {\n      magicEnum = InnerEnum.B;\n    }\n    switch (magicEnum) {\n      case B:\n        System.out.println(\"b\");\n        break;\n      case A:\n        System.out.println(\"a\");\n        break;\n      default:\n        break;\n    }\n    if (magicEnum == InnerEnum.A) System.out.println(\"a\");\n    else if (magicEnum == InnerEnum.C) System.out.println(\"b\");\n    else System.out.println(\"c\");\n  }\n\n  public static void testAnon() {\n    InnerInterface<String, String> fn =\n        new InnerInterface<String, String>() {\n          @Override\n          public String apply(String s) {\n            return s + \"b\";\n          }\n        };\n    System.out.println(fn.apply(\"a\"));\n  }\n\n  public static String app() {\n    int a = 42;\n    InnerStaticClass.innerStaticMethod();\n    InnerClasses innerClasses = new InnerClasses(a);\n    InnerClass innerClass = innerClasses.new InnerClass(a);\n    innerClass.innerMethod();\n    System.out.println(runInnerInterface(innerClass, a));\n    testEnum(InnerEnum.A);\n    testAnon();\n    return \"\";\n  }\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "runInnerInterface",
                expected: json!([
                    {"filePath": "src/main/java/minimized/InnerClasses.java", "startLine": 54, "edges": [
                        {"symbol": {"name": "apply", "filePath": "src/main/java/minimized/InnerClasses.java", "startLine": 23}, "filePath": "src/main/java/minimized/InnerClasses.java", "line": 55},
                    ]},
                ]),
                file: Some("src/main/java/minimized/InnerClasses.java"),
                short_names: true,
                definition_line: Some(54),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "testAnon",
                expected: json!([
                    {"filePath": "src/main/java/minimized/InnerClasses.java", "startLine": 77, "edges": [
                        {"symbol": {"name": "apply", "filePath": "src/main/java/minimized/InnerClasses.java", "startLine": 23}, "filePath": "src/main/java/minimized/InnerClasses.java", "line": 85},
                    ]},
                ]),
                file: Some("src/main/java/minimized/InnerClasses.java"),
                short_names: true,
                definition_line: Some(77),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "app",
                expected: json!([
                    {"filePath": "src/main/java/minimized/InnerClasses.java", "startLine": 88, "edges": [
                        {"symbol": {"name": "innerStaticMethod", "filePath": "src/main/java/minimized/InnerClasses.java", "startLine": 34}, "filePath": "src/main/java/minimized/InnerClasses.java", "line": 90},
                        {"symbol": {"name": "innerMethod", "filePath": "src/main/java/minimized/InnerClasses.java", "startLine": 44}, "filePath": "src/main/java/minimized/InnerClasses.java", "line": 93},
                        {"symbol": {"name": "runInnerInterface", "filePath": "src/main/java/minimized/InnerClasses.java", "startLine": 54}, "filePath": "src/main/java/minimized/InnerClasses.java", "line": 94},
                        {"symbol": {"name": "testEnum", "filePath": "src/main/java/minimized/InnerClasses.java", "startLine": 58}, "filePath": "src/main/java/minimized/InnerClasses.java", "line": 95},
                        {"symbol": {"name": "testAnon", "filePath": "src/main/java/minimized/InnerClasses.java", "startLine": 77}, "filePath": "src/main/java/minimized/InnerClasses.java", "line": 96},
                    ]},
                ]),
                file: Some("src/main/java/minimized/InnerClasses.java"),
                short_names: true,
                definition_line: Some(88),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "apply",
                expected: json!([
                    {"filePath": "src/main/java/minimized/InnerClasses.java", "startLine": 23, "edges": [
                        {"symbol": {"name": "runInnerInterface", "filePath": "src/main/java/minimized/InnerClasses.java", "startLine": 54}, "filePath": "src/main/java/minimized/InnerClasses.java", "line": 55},
                        {"symbol": {"name": "testAnon", "filePath": "src/main/java/minimized/InnerClasses.java", "startLine": 77}, "filePath": "src/main/java/minimized/InnerClasses.java", "line": 85},
                    ]},
                ]),
                file: Some("src/main/java/minimized/InnerClasses.java"),
                short_names: true,
                definition_line: Some(23),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "innerStaticMethod",
                expected: json!([
                    {"filePath": "src/main/java/minimized/InnerClasses.java", "startLine": 34, "edges": [
                        {"symbol": {"name": "app", "filePath": "src/main/java/minimized/InnerClasses.java", "startLine": 88}, "filePath": "src/main/java/minimized/InnerClasses.java", "line": 90},
                    ]},
                ]),
                file: Some("src/main/java/minimized/InnerClasses.java"),
                short_names: true,
                definition_line: Some(34),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "innerMethod",
                expected: json!([
                    {"filePath": "src/main/java/minimized/InnerClasses.java", "startLine": 44, "edges": [
                        {"symbol": {"name": "app", "filePath": "src/main/java/minimized/InnerClasses.java", "startLine": 88}, "filePath": "src/main/java/minimized/InnerClasses.java", "line": 93},
                    ]},
                ]),
                file: Some("src/main/java/minimized/InnerClasses.java"),
                short_names: true,
                definition_line: Some(44),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "runInnerInterface",
                expected: json!([
                    {"filePath": "src/main/java/minimized/InnerClasses.java", "startLine": 54, "edges": [
                        {"symbol": {"name": "app", "filePath": "src/main/java/minimized/InnerClasses.java", "startLine": 88}, "filePath": "src/main/java/minimized/InnerClasses.java", "line": 94},
                    ]},
                ]),
                file: Some("src/main/java/minimized/InnerClasses.java"),
                short_names: true,
                definition_line: Some(54),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "testEnum",
                expected: json!([
                    {"filePath": "src/main/java/minimized/InnerClasses.java", "startLine": 58, "edges": [
                        {"symbol": {"name": "app", "filePath": "src/main/java/minimized/InnerClasses.java", "startLine": 88}, "filePath": "src/main/java/minimized/InnerClasses.java", "line": 95},
                    ]},
                ]),
                file: Some("src/main/java/minimized/InnerClasses.java"),
                short_names: true,
                definition_line: Some(58),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "testAnon",
                expected: json!([
                    {"filePath": "src/main/java/minimized/InnerClasses.java", "startLine": 77, "edges": [
                        {"symbol": {"name": "app", "filePath": "src/main/java/minimized/InnerClasses.java", "startLine": 88}, "filePath": "src/main/java/minimized/InnerClasses.java", "line": 96},
                    ]},
                ]),
                file: Some("src/main/java/minimized/InnerClasses.java"),
                short_names: true,
                definition_line: Some(77),
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/sourcegraph/scip-clang/tree/14423f07d164e640b7ffe2de937789653d63708c/test/index/functions
// Snapshot: test/index/functions/functions.snapshot.cc:16
// Target: [..] overloaded_func(d4f767463ce0a6b3).
// Snapshot: test/index/functions/functions.snapshot.cc:27
// Target: [..] detail/shadowed_func(49f6e7a06ebc5aa8).
// Snapshot: test/index/functions/functions.snapshot.cc:34
// Target: [..] shadowed_func(49f6e7a06ebc5aa8).
#[tokio::test]
async fn scip_cpp_functions() {
    check_case(Case {
        name: "scip_cpp_functions",
        files: vec![
            SourceFile {
                path: "functions.cc",
                source: "void top_level_func() {}\n\nnamespace my_namespace {\n  void func_in_namespace() {}\n}\n\nvoid overloaded_func(int) {}\nvoid overloaded_func(const char *) {\n  overloaded_func(32);\n}\n\nvoid shadowed_func() {}\n\nnamespace detail {\n  void shadowed_func() {\n    shadowed_func();\n  }\n}\n\nvoid use_outer() {\n  shadowed_func();\n}\n\n// check that the same canonical type produces the same hash\nusing IntAlias = int;\nvoid int_to_void_fn(int) {}\nvoid same_hash_as_previous(IntAlias) {}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "overloaded_func",
                expected: json!([
                    {"filePath": "functions.cc", "startLine": 8, "edges": [
                        {"symbol": {"name": "overloaded_func", "filePath": "functions.cc", "startLine": 7}, "filePath": "functions.cc", "line": 9},
                    ]},
                ]),
                file: Some("functions.cc"),
                short_names: true,
                definition_line: Some(8),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "shadowed_func",
                expected: json!([
                    {"filePath": "functions.cc", "startLine": 15, "edges": [
                        {"symbol": {"name": "shadowed_func", "filePath": "functions.cc", "startLine": 15}, "filePath": "functions.cc", "line": 16},
                    ]},
                ]),
                file: Some("functions.cc"),
                short_names: true,
                definition_line: Some(15),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "use_outer",
                expected: json!([
                    {"filePath": "functions.cc", "startLine": 20, "edges": [
                        {"symbol": {"name": "shadowed_func", "filePath": "functions.cc", "startLine": 12}, "filePath": "functions.cc", "line": 21},
                    ]},
                ]),
                file: Some("functions.cc"),
                short_names: true,
                definition_line: Some(20),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "overloaded_func",
                expected: json!([
                    {"filePath": "functions.cc", "startLine": 7, "edges": [
                        {"symbol": {"name": "overloaded_func", "filePath": "functions.cc", "startLine": 8}, "filePath": "functions.cc", "line": 9},
                    ]},
                ]),
                file: Some("functions.cc"),
                short_names: true,
                definition_line: Some(7),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "shadowed_func",
                expected: json!([
                    {"filePath": "functions.cc", "startLine": 15, "edges": [
                        {"symbol": {"name": "shadowed_func", "filePath": "functions.cc", "startLine": 15}, "filePath": "functions.cc", "line": 16},
                    ]},
                ]),
                file: Some("functions.cc"),
                short_names: true,
                definition_line: Some(15),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "shadowed_func",
                expected: json!([
                    {"filePath": "functions.cc", "startLine": 12, "edges": [
                        {"symbol": {"name": "use_outer", "filePath": "functions.cc", "startLine": 20}, "filePath": "functions.cc", "line": 21},
                    ]},
                ]),
                file: Some("functions.cc"),
                short_names: true,
                definition_line: Some(12),
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/sourcegraph/scip-clang/tree/14423f07d164e640b7ffe2de937789653d63708c/test/index/functions
// Snapshot: test/index/functions/methods.snapshot.cc:9
// Target: [..] S0#v2(49f6e7a06ebc5aa8).
// Snapshot: test/index/functions/methods.snapshot.cc:29
// Target: [..] S1_0#v2(49f6e7a06ebc5aa8).
// Snapshot: test/index/functions/methods.snapshot.cc:42
// Target: [..] S0#v1(49f6e7a06ebc5aa8).
// Snapshot: test/index/functions/methods.snapshot.cc:46
// Target: [..] S0#v5(49f6e7a06ebc5aa8).
// Snapshot: test/index/functions/methods.snapshot.cc:60
// Target: [..] S2#v5(49f6e7a06ebc5aa8).
// Snapshot: test/index/functions/methods.snapshot.cc:201
// Target: [..] u/Z0#f(d4f767463ce0a6b3).
// Snapshot: test/index/functions/methods.snapshot.cc:204
// Target: [..] u/Z0#f(3e454b8ccf442868).
#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "Keep the upstream source and its snapshot-derived queries together"
)]
async fn scip_cpp_methods() {
    check_case(Case {
        name: "scip_cpp_methods",
        files: vec![
            SourceFile {
                path: "methods.cc",
                source: "// Virtual methods and inheritance\n\nstruct S0 {\n  virtual void v1() {\n    v2();\n  }\n  virtual void v2() {}\n  virtual void v3() {}\n  virtual void v4() {}\n  virtual void v5() = 0;\n\n};\n\nstruct S1_0: S0 {\n  virtual void v1() override { v2(); }\n  virtual void v2() /*override*/ {}\n};\n\nstruct S1_1: S0 {\n  virtual void v2() override { v1(); }\n  virtual void v3() override { v5(); }\n};\n\nstruct S2 final: S1_0, S1_1 {\n  virtual void v1() override { v5(); }\n  virtual void v2() override {}\n  virtual void v3() override {}\n  virtual void v4() override {}\n  virtual void v5() override {}\n};\n\n// Method forward declarations\n\nstruct A0 {\n  void f();\n  static void g();\n  struct B0 {\n    void f();\n    static void g();\n  };\n  friend bool operator==(const A0 &, const A0 &);\n};\n\nvoid A0::f() {}\nvoid A0::g() {}\nvoid A0::B0::f() {}\nvoid A0::B0::g() {}\n\n// Not A::operator==\nbool operator==(const A0 &, const A0 &) { return true; }\n\n// Static methods\n\nstruct Z0 {\n  static void f();\n  void g() { f(); }\n};\n\nstruct Z1: Z0 {\n  void h() {\n    f();\n    Z1::f();\n    Z0::f();\n  }\n};\n\n// Member function pointer\n\nstruct M0 {\n  void f() {}\n};\n\nvoid test_member_pointer() {\n  void (M0::*p)() = &M0::f;\n  M0 m{};\n  (m.*p)();\n}\n\n// Using-declarations making methods public:\n\nnamespace u {\n  struct Z0 {\n  protected:\n    void f(int) {}\n    void f(int, int) {}\n  };\n\n  struct Z1: Z0 {\n    using Z0::f;\n  };\n\n  void use_made_public(Z1 z1) {\n    z1.f(0);\n    z1.f(0, 0);\n  }\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "v1",
                expected: json!([
                    {"filePath": "methods.cc", "startLine": 4, "edges": [
                        {"symbol": {"name": "v2", "filePath": "methods.cc", "startLine": 7}, "filePath": "methods.cc", "line": 5},
                    ]},
                ]),
                file: Some("methods.cc"),
                short_names: true,
                definition_line: Some(4),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "v1",
                expected: json!([
                    {"filePath": "methods.cc", "startLine": 15, "edges": [
                        {"symbol": {"name": "v2", "filePath": "methods.cc", "startLine": 16}, "filePath": "methods.cc", "line": 15},
                    ]},
                ]),
                file: Some("methods.cc"),
                short_names: true,
                definition_line: Some(15),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "v2",
                expected: json!([
                    {"filePath": "methods.cc", "startLine": 20, "edges": [
                        {"symbol": {"name": "v1", "filePath": "methods.cc", "startLine": 4}, "filePath": "methods.cc", "line": 20},
                    ]},
                ]),
                file: Some("methods.cc"),
                short_names: true,
                definition_line: Some(20),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "v3",
                expected: json!([
                    {"filePath": "methods.cc", "startLine": 21, "edges": [
                        {"symbol": {"name": "v5", "filePath": "methods.cc", "startLine": 10}, "filePath": "methods.cc", "line": 21},
                    ]},
                ]),
                file: Some("methods.cc"),
                short_names: true,
                definition_line: Some(21),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "v1",
                expected: json!([
                    {"filePath": "methods.cc", "startLine": 25, "edges": [
                        {"symbol": {"name": "v5", "filePath": "methods.cc", "startLine": 29}, "filePath": "methods.cc", "line": 25},
                    ]},
                ]),
                file: Some("methods.cc"),
                short_names: true,
                definition_line: Some(25),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "use_made_public",
                expected: json!([
                    {"filePath": "methods.cc", "startLine": 92, "edges": [
                        {"symbol": {"name": "f", "filePath": "methods.cc", "startLine": 84}, "filePath": "methods.cc", "line": 93},
                        {"symbol": {"name": "f", "filePath": "methods.cc", "startLine": 85}, "filePath": "methods.cc", "line": 94},
                    ]},
                ]),
                file: Some("methods.cc"),
                short_names: true,
                definition_line: Some(92),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "v2",
                expected: json!([
                    {"filePath": "methods.cc", "startLine": 7, "edges": [
                        {"symbol": {"name": "v1", "filePath": "methods.cc", "startLine": 4}, "filePath": "methods.cc", "line": 5},
                    ]},
                ]),
                file: Some("methods.cc"),
                short_names: true,
                definition_line: Some(7),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "v2",
                expected: json!([
                    {"filePath": "methods.cc", "startLine": 16, "edges": [
                        {"symbol": {"name": "v1", "filePath": "methods.cc", "startLine": 15}, "filePath": "methods.cc", "line": 15},
                    ]},
                ]),
                file: Some("methods.cc"),
                short_names: true,
                definition_line: Some(16),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "v1",
                expected: json!([
                    {"filePath": "methods.cc", "startLine": 4, "edges": [
                        {"symbol": {"name": "v2", "filePath": "methods.cc", "startLine": 20}, "filePath": "methods.cc", "line": 20},
                    ]},
                ]),
                file: Some("methods.cc"),
                short_names: true,
                definition_line: Some(4),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "v5",
                expected: json!([
                    {"filePath": "methods.cc", "startLine": 10, "edges": [
                        {"symbol": {"name": "v3", "filePath": "methods.cc", "startLine": 21}, "filePath": "methods.cc", "line": 21},
                    ]},
                ]),
                file: Some("methods.cc"),
                short_names: true,
                definition_line: Some(10),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "v5",
                expected: json!([
                    {"filePath": "methods.cc", "startLine": 29, "edges": [
                        {"symbol": {"name": "v1", "filePath": "methods.cc", "startLine": 25}, "filePath": "methods.cc", "line": 25},
                    ]},
                ]),
                file: Some("methods.cc"),
                short_names: true,
                definition_line: Some(29),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "f",
                expected: json!([
                    {"filePath": "methods.cc", "startLine": 84, "edges": [
                        {"symbol": {"name": "use_made_public", "filePath": "methods.cc", "startLine": 92}, "filePath": "methods.cc", "line": 93},
                    ]},
                ]),
                file: Some("methods.cc"),
                short_names: true,
                definition_line: Some(84),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "f",
                expected: json!([
                    {"filePath": "methods.cc", "startLine": 85, "edges": [
                        {"symbol": {"name": "use_made_public", "filePath": "methods.cc", "startLine": 92}, "filePath": "methods.cc", "line": 94},
                    ]},
                ]),
                file: Some("methods.cc"),
                short_names: true,
                definition_line: Some(85),
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/sourcegraph/scip-clang/tree/14423f07d164e640b7ffe2de937789653d63708c/test/index/functions
// Snapshot: test/index/functions/body.snapshot.cc:18
// Target: [..] f(49f6e7a06ebc5aa8).C#g(49f6e7a06ebc5aa8).
#[tokio::test]
async fn scip_cpp_body() {
    check_case(Case {
        name: "scip_cpp_body",
        files: vec![
            SourceFile {
                path: "body.cc",
                source: "void f() {\n  struct C {\n    int plain_field;\n\n    void g() {};\n  };\n\n  (void)C().plain_field;\n  C().g();\n\n  int x = 0;\n  (void)(2 * x);\n}\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "f",
                expected: json!([
                    {"filePath": "body.cc", "startLine": 1, "edges": [
                        {"symbol": {"name": "g", "filePath": "body.cc", "startLine": 5}, "filePath": "body.cc", "line": 9},
                    ]},
                ]),
                file: Some("body.cc"),
                short_names: true,
                definition_line: Some(1),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "g",
                expected: json!([
                    {"filePath": "body.cc", "startLine": 5, "edges": [
                        {"symbol": {"name": "f", "filePath": "body.cc", "startLine": 1}, "filePath": "body.cc", "line": 9},
                    ]},
                ]),
                file: Some("body.cc"),
                short_names: true,
                definition_line: Some(5),
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/sourcegraph/scip-clang/tree/14423f07d164e640b7ffe2de937789653d63708c/test/index/functions
// Snapshot: test/index/functions/template_body.snapshot.cc:26
// Target: [..] f(49f6e7a06ebc5aa8).C#g(49f6e7a06ebc5aa8).
// Snapshot: test/index/functions/template_body.snapshot.cc:48
// Target: [..] Z#f0(49f6e7a06ebc5aa8).
// Snapshot: test/index/functions/template_body.snapshot.cc:56
// Target: [..] Z#f0(49f6e7a06ebc5aa8).
// Snapshot: test/index/functions/template_body.snapshot.cc:64
// Target: [..] Z#g0(49f6e7a06ebc5aa8).
// Snapshot: test/index/functions/template_body.snapshot.cc:79
// Target: [..] Z#f0(49f6e7a06ebc5aa8).
// Snapshot: test/index/functions/template_body.snapshot.cc:87
// Target: [..] Z#f0(49f6e7a06ebc5aa8).
#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "Keep the upstream source and its snapshot-derived queries together"
)]
async fn scip_cpp_template_body() {
    check_case(Case {
        name: "scip_cpp_template_body",
        files: vec![
            SourceFile {
                path: "template_body.cc",
                source: "template <typename T>\nvoid f() {\n  struct C {\n    int plain_field;\n    T dependent_field;\n\n    void g() {};\n  };\n\n  (void)C().plain_field;\n  (void)C().dependent_field;\n  C().g();\n\n  int x = 0;\n  (void)(2 * x);\n\n  // The following are not allowed:\n  // - Templated function-local classes\n  // - Templates inside function-local classes\n}\n\ntemplate <typename T>\nstruct Z {\n  void f0() {}\n\n  void f1() {\n    f0();\n  }\n\n  template <typename U>\n  void g0() {\n    f0();\n  }\n\n  template <typename U>\n  void g1() {\n    g0<U>();\n  }\n};\n\ntemplate <typename T>\nstruct ZZ : Z<T> {\n  void ff0() {\n    this->f0();\n  }\n\n  template <typename U>\n  void gg0() {\n    this->f0();\n    this->template g0<U>();\n  }\n};",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "f",
                expected: json!([
                    {"filePath": "template_body.cc", "startLine": 2, "edges": [
                        {"symbol": {"name": "g", "filePath": "template_body.cc", "startLine": 7}, "filePath": "template_body.cc", "line": 12},
                    ]},
                ]),
                file: Some("template_body.cc"),
                short_names: true,
                definition_line: Some(2),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "f1",
                expected: json!([
                    {"filePath": "template_body.cc", "startLine": 26, "edges": [
                        {"symbol": {"name": "f0", "filePath": "template_body.cc", "startLine": 24}, "filePath": "template_body.cc", "line": 27},
                    ]},
                ]),
                file: Some("template_body.cc"),
                short_names: true,
                definition_line: Some(26),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "g0",
                expected: json!([
                    {"filePath": "template_body.cc", "startLine": 31, "edges": [
                        {"symbol": {"name": "f0", "filePath": "template_body.cc", "startLine": 24}, "filePath": "template_body.cc", "line": 32},
                    ]},
                ]),
                file: Some("template_body.cc"),
                short_names: true,
                definition_line: Some(31),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "g1",
                expected: json!([
                    {"filePath": "template_body.cc", "startLine": 36, "edges": [
                        {"symbol": {"name": "g0", "filePath": "template_body.cc", "startLine": 31}, "filePath": "template_body.cc", "line": 37},
                    ]},
                ]),
                file: Some("template_body.cc"),
                short_names: true,
                definition_line: Some(36),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "ff0",
                expected: json!([
                    {"filePath": "template_body.cc", "startLine": 43, "edges": [
                        {"symbol": {"name": "f0", "filePath": "template_body.cc", "startLine": 24}, "filePath": "template_body.cc", "line": 44},
                    ]},
                ]),
                file: Some("template_body.cc"),
                short_names: true,
                definition_line: Some(43),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "gg0",
                expected: json!([
                    {"filePath": "template_body.cc", "startLine": 48, "edges": [
                        {"symbol": {"name": "f0", "filePath": "template_body.cc", "startLine": 24}, "filePath": "template_body.cc", "line": 49},
                    ]},
                ]),
                file: Some("template_body.cc"),
                short_names: true,
                definition_line: Some(48),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "g",
                expected: json!([
                    {"filePath": "template_body.cc", "startLine": 7, "edges": [
                        {"symbol": {"name": "f", "filePath": "template_body.cc", "startLine": 2}, "filePath": "template_body.cc", "line": 12},
                    ]},
                ]),
                file: Some("template_body.cc"),
                short_names: true,
                definition_line: Some(7),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "f0",
                expected: json!([
                    {"filePath": "template_body.cc", "startLine": 24, "edges": [
                        {"symbol": {"name": "f1", "filePath": "template_body.cc", "startLine": 26}, "filePath": "template_body.cc", "line": 27},
                        {"symbol": {"name": "g0", "filePath": "template_body.cc", "startLine": 31}, "filePath": "template_body.cc", "line": 32},
                        {"symbol": {"name": "ff0", "filePath": "template_body.cc", "startLine": 43}, "filePath": "template_body.cc", "line": 44},
                        {"symbol": {"name": "gg0", "filePath": "template_body.cc", "startLine": 48}, "filePath": "template_body.cc", "line": 49},
                    ]},
                ]),
                file: Some("template_body.cc"),
                short_names: true,
                definition_line: Some(24),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "g0",
                expected: json!([
                    {"filePath": "template_body.cc", "startLine": 31, "edges": [
                        {"symbol": {"name": "g1", "filePath": "template_body.cc", "startLine": 36}, "filePath": "template_body.cc", "line": 37},
                    ]},
                ]),
                file: Some("template_body.cc"),
                short_names: true,
                definition_line: Some(31),
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/sourcegraph/scip-clang/tree/14423f07d164e640b7ffe2de937789653d63708c/test/index/functions
// Snapshot: test/index/functions/templates.snapshot.cc:29
// Target: [..] T0#f0(9b289cee16747614).
// Snapshot: test/index/functions/templates.snapshot.cc:58
// Target: [..] h0(9b289cee16747614).
// Snapshot: test/index/functions/templates.snapshot.cc:66
// Target: [..] T0#f0(9b289cee16747614).
// Snapshot: test/index/functions/templates.snapshot.cc:69
// Target: [..] T1#f1(9b289cee16747614).
// Snapshot: test/index/functions/templates.snapshot.cc:75
// Target: [..] T0#f0(9b289cee16747614).
// Snapshot: test/index/functions/templates.snapshot.cc:91
// Target: [..] h0(9b289cee16747614).
// Snapshot: test/index/functions/templates.snapshot.cc:93
// Target: [..] h0(9b289cee16747614).
// Snapshot: test/index/functions/templates.snapshot.cc:95
// Target: [..] h1(9b289cee16747614).
// Snapshot: test/index/functions/templates.snapshot.cc:97
// Target: [..] h1(9b289cee16747614).
// Snapshot: test/index/functions/templates.snapshot.cc:123
// Target: [..] Q0#f(49f6e7a06ebc5aa8).
// Snapshot: test/index/functions/templates.snapshot.cc:142
// Target: [..] Q0#f(49f6e7a06ebc5aa8).
// Snapshot: test/index/functions/templates.snapshot.cc:169
// Target: [..] FwdDecl2#whatever(49f6e7a06ebc5aa8).
#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "Keep the upstream source and its snapshot-derived queries together"
)]
async fn scip_cpp_templates() {
    check_case(Case {
        name: "scip_cpp_templates",
        files: vec![
            SourceFile {
                path: "templates.cc",
                source: "template <typename T>\nstruct T0 {\n  void f0(T) {}\n\n  template <typename U>\n  void g0(U) {}\n};\n\ntemplate <typename T>\nstruct T1: T0<T> {\n  void f1(T t) {\n    this->f0(t);\n  }\n\n  template <typename U>\n  void g1(U u) {\n    this->template g0<U>(u);\n  }\n};\n\ntemplate <typename H>\nvoid h0(H) {}\n\ntemplate <typename H>\nvoid h1(H h) { h0<H>(h); }\n\nvoid test_template() {\n  T0<int>().f0(0);\n  T1<int>().f1(0);\n  auto t1 = T1<int>();\n  t1.f0(0);\n\n  T0<int>().g0<int>(0);\n  T1<int>().g1<unsigned>(0);\n  auto t1_ = T1<int>();\n  t1_.g0<char>(0);\n\n  h0<int>(0);\n  h0<void *>(0);\n  h1<int>(0);\n  h1<char>(0);\n}\n\ntemplate <typename T>\nstruct Q0 {\n  void f() {}\n};\n\ntemplate <typename T>\nstruct Q1: Q0<T> {\n  using Base1 = Q0<T>;\n  using Base1::f;\n  void g() { f(); }\n};\n\ntemplate <typename T>\nstruct Q2: Q1<T> {\n  using Base2 = Q1<T>;\n  using Base2::f;\n  void h() { f(); }\n};\n\ntemplate <typename T>\nstruct FwdDecl1;\n\ntemplate <typename T>\nstruct FwdDecl2;\n\ntemplate <typename X>\nvoid f(FwdDecl1<X> &a1, FwdDecl2<X> &a2) {\n  a1.whatever(); // No code nav, sorry\n  a2.whatever();\n}\n\ntemplate <typename T>\nstruct FwdDecl2 {\n  void whatever() {}\n};",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "f1",
                expected: json!([
                    {"filePath": "templates.cc", "startLine": 11, "edges": [
                        {"symbol": {"name": "f0", "filePath": "templates.cc", "startLine": 3}, "filePath": "templates.cc", "line": 12},
                    ]},
                ]),
                file: Some("templates.cc"),
                short_names: true,
                definition_line: Some(11),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "h1",
                expected: json!([
                    {"filePath": "templates.cc", "startLine": 25, "edges": [
                        {"symbol": {"name": "h0", "filePath": "templates.cc", "startLine": 22}, "filePath": "templates.cc", "line": 25},
                    ]},
                ]),
                file: Some("templates.cc"),
                short_names: true,
                definition_line: Some(25),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "test_template",
                expected: json!([
                    {"filePath": "templates.cc", "startLine": 27, "edges": [
                        {"symbol": {"name": "f0", "filePath": "templates.cc", "startLine": 3}, "filePath": "templates.cc", "line": 28},
                        {"symbol": {"name": "f1", "filePath": "templates.cc", "startLine": 11}, "filePath": "templates.cc", "line": 29},
                        {"symbol": {"name": "f0", "filePath": "templates.cc", "startLine": 3}, "filePath": "templates.cc", "line": 31},
                        {"symbol": {"name": "h0", "filePath": "templates.cc", "startLine": 22}, "filePath": "templates.cc", "line": 38},
                        {"symbol": {"name": "h0", "filePath": "templates.cc", "startLine": 22}, "filePath": "templates.cc", "line": 39},
                        {"symbol": {"name": "h1", "filePath": "templates.cc", "startLine": 25}, "filePath": "templates.cc", "line": 40},
                        {"symbol": {"name": "h1", "filePath": "templates.cc", "startLine": 25}, "filePath": "templates.cc", "line": 41},
                    ]},
                ]),
                file: Some("templates.cc"),
                short_names: true,
                definition_line: Some(27),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "g",
                expected: json!([
                    {"filePath": "templates.cc", "startLine": 53, "edges": [
                        {"symbol": {"name": "f", "filePath": "templates.cc", "startLine": 46}, "filePath": "templates.cc", "line": 53},
                    ]},
                ]),
                file: Some("templates.cc"),
                short_names: true,
                definition_line: Some(53),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "h",
                expected: json!([
                    {"filePath": "templates.cc", "startLine": 60, "edges": [
                        {"symbol": {"name": "f", "filePath": "templates.cc", "startLine": 46}, "filePath": "templates.cc", "line": 60},
                    ]},
                ]),
                file: Some("templates.cc"),
                short_names: true,
                definition_line: Some(60),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callees",
                symbol: "f",
                expected: json!([
                    {"filePath": "templates.cc", "startLine": 70, "edges": [
                        {"symbol": {"name": "whatever", "filePath": "templates.cc", "startLine": 77}, "filePath": "templates.cc", "line": 72},
                    ]},
                ]),
                file: Some("templates.cc"),
                short_names: true,
                definition_line: Some(70),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "f0",
                expected: json!([
                    {"filePath": "templates.cc", "startLine": 3, "edges": [
                        {"symbol": {"name": "f1", "filePath": "templates.cc", "startLine": 11}, "filePath": "templates.cc", "line": 12},
                        {"symbol": {"name": "test_template", "filePath": "templates.cc", "startLine": 27}, "filePath": "templates.cc", "line": 28},
                        {"symbol": {"name": "test_template", "filePath": "templates.cc", "startLine": 27}, "filePath": "templates.cc", "line": 31},
                    ]},
                ]),
                file: Some("templates.cc"),
                short_names: true,
                definition_line: Some(3),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "h0",
                expected: json!([
                    {"filePath": "templates.cc", "startLine": 22, "edges": [
                        {"symbol": {"name": "h1", "filePath": "templates.cc", "startLine": 25}, "filePath": "templates.cc", "line": 25},
                        {"symbol": {"name": "test_template", "filePath": "templates.cc", "startLine": 27}, "filePath": "templates.cc", "line": 38},
                        {"symbol": {"name": "test_template", "filePath": "templates.cc", "startLine": 27}, "filePath": "templates.cc", "line": 39},
                    ]},
                ]),
                file: Some("templates.cc"),
                short_names: true,
                definition_line: Some(22),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "f1",
                expected: json!([
                    {"filePath": "templates.cc", "startLine": 11, "edges": [
                        {"symbol": {"name": "test_template", "filePath": "templates.cc", "startLine": 27}, "filePath": "templates.cc", "line": 29},
                    ]},
                ]),
                file: Some("templates.cc"),
                short_names: true,
                definition_line: Some(11),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "h1",
                expected: json!([
                    {"filePath": "templates.cc", "startLine": 25, "edges": [
                        {"symbol": {"name": "test_template", "filePath": "templates.cc", "startLine": 27}, "filePath": "templates.cc", "line": 40},
                        {"symbol": {"name": "test_template", "filePath": "templates.cc", "startLine": 27}, "filePath": "templates.cc", "line": 41},
                    ]},
                ]),
                file: Some("templates.cc"),
                short_names: true,
                definition_line: Some(25),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "f",
                expected: json!([
                    {"filePath": "templates.cc", "startLine": 46, "edges": [
                        {"symbol": {"name": "g", "filePath": "templates.cc", "startLine": 53}, "filePath": "templates.cc", "line": 53},
                        {"symbol": {"name": "h", "filePath": "templates.cc", "startLine": 60}, "filePath": "templates.cc", "line": 60},
                    ]},
                ]),
                file: Some("templates.cc"),
                short_names: true,
                definition_line: Some(46),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "whatever",
                expected: json!([
                    {"filePath": "templates.cc", "startLine": 77, "edges": [
                        {"symbol": {"name": "f", "filePath": "templates.cc", "startLine": 70}, "filePath": "templates.cc", "line": 72},
                    ]},
                ]),
                file: Some("templates.cc"),
                short_names: true,
                definition_line: Some(77),
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/rust-lang/rust-analyzer/blob/2a362a41a8eb6e090f1a12d9b712df85af6f0c33/crates/rust-analyzer/src/cli/scip.rs#L589
// Snapshot: crates/rust-analyzer/src/cli/scip.rs:589
// Target: rust-analyzer cargo foo 0.1.0 lib/example_mod/func().
#[tokio::test]
async fn scip_rust_basic() {
    check_case(Case {
        name: "scip_rust_basic",
        files: vec![
            SourceFile {
                path: "workspace/lib.rs",
                source: "use foo::example_mod::func;\nfn main() {\n    func();\n}\n",
            },
            SourceFile {
                path: "foo/lib.rs",
                source: "pub mod example_mod {\n    pub fn func() {}\n}\n",
            },
            SourceFile {
                path: "Cargo.toml",
                source: "[workspace]\nmembers = [\"workspace\", \"foo\"]\nresolver = \"2\"\n",
            },
            SourceFile {
                path: "workspace/Cargo.toml",
                source: "[package]\nname = \"main\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[lib]\npath = \"lib.rs\"\n[dependencies]\nfoo = { path = \"../foo\" }\n",
            },
            SourceFile {
                path: "foo/Cargo.toml",
                source: "[package]\nname = \"foo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[lib]\npath = \"lib.rs\"\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "main",
                expected: json!([
                    {"filePath": "workspace/lib.rs", "startLine": 2, "edges": [
                        {"filePath": "workspace/lib.rs", "line": 3, "symbol": {"name": "func", "filePath": "foo/lib.rs", "startLine": 2}},
                    ]},
                ]),
                file: Some("workspace/lib.rs"),
                short_names: true,
                definition_line: Some(2),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "func",
                expected: json!([
                    {"filePath": "foo/lib.rs", "startLine": 2, "edges": [
                        {"filePath": "workspace/lib.rs", "line": 3, "symbol": {"name": "main", "filePath": "workspace/lib.rs", "startLine": 2}},
                    ]},
                ]),
                file: Some("foo/lib.rs"),
                short_names: true,
                definition_line: Some(2),
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}

// https://github.com/rust-lang/rust-analyzer/blob/2a362a41a8eb6e090f1a12d9b712df85af6f0c33/crates/rust-analyzer/src/cli/scip.rs#L607
// Snapshot: crates/rust-analyzer/src/cli/scip.rs:607
// Target: rust-analyzer cargo main . lib/impl#[S][`AddAssign<Self>`]add_assign().
#[tokio::test]
async fn scip_rust_operator_overload() {
    check_case(Case {
        name: "scip_rust_operator_overload",
        files: vec![
            SourceFile {
                path: "workspace/lib.rs",
                source: "use core::ops::AddAssign;\n\nstruct S;\n\nimpl AddAssign for S {\n    fn add_assign(&mut self, _rhs: Self) {}\n}\n\nfn main() {\n    let mut s = S;\n    s += S;\n}\n",
            },
            SourceFile {
                path: "workspace/Cargo.toml",
                source: "[package]\nname = \"main\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[lib]\npath = \"lib.rs\"\n",
            },
        ],
        queries: vec![
            Query {
                operation: "callees",
                symbol: "main",
                expected: json!([
                    {"filePath": "workspace/lib.rs", "startLine": 9, "edges": [
                        {"filePath": "workspace/lib.rs", "line": 11, "symbol": {"name": "add_assign", "filePath": "workspace/lib.rs", "startLine": 6}},
                    ]},
                ]),
                file: Some("workspace/lib.rs"),
                short_names: true,
                definition_line: Some(9),
                check: Check::Contains,
                ..Query::default()
            },
            Query {
                operation: "callers",
                symbol: "add_assign",
                expected: json!([
                    {"filePath": "workspace/lib.rs", "startLine": 6, "edges": [
                        {"filePath": "workspace/lib.rs", "line": 11, "symbol": {"name": "main", "filePath": "workspace/lib.rs", "startLine": 9}},
                    ]},
                ]),
                file: Some("workspace/lib.rs"),
                short_names: true,
                definition_line: Some(6),
                check: Check::Contains,
                ..Query::default()
            },
        ],
    }).await;
}
