import { createCFamilyAdapter } from "../families/c-family.js";

export const CPP_ADAPTER = createCFamilyAdapter(
  "cpp",
  [
    "alias_declaration",
    "declaration",
    "field_declaration",
    "function_definition",
    "macro_type_specifier",
    "class_specifier",
    "struct_specifier",
    "union_specifier",
    "enum_specifier",
  ],
  [
    "namespace_definition",
    "class_specifier",
    "struct_specifier",
    "union_specifier",
  ],
);
