import { createCFamilyAdapter } from "../families/c-family.js";

export const C_ADAPTER = createCFamilyAdapter(
  "c",
  [
    "declaration",
    "field_declaration",
    "function_definition",
    "macro_type_specifier",
    "struct_specifier",
    "union_specifier",
    "enum_specifier",
    "type_definition",
  ],
  ["struct_specifier", "union_specifier"],
);
