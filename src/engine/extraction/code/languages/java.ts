import { createNameFieldAdapter } from "../families/name-field.js";

export const JAVA_ADAPTER = createNameFieldAdapter(
  "java",
  [
    "annotation_type_declaration",
    "class_declaration",
    "constructor_declaration",
    "enum_declaration",
    "interface_declaration",
    "method_declaration",
    "record_declaration",
  ],
  [
    "annotation_type_declaration",
    "class_declaration",
    "enum_declaration",
    "interface_declaration",
    "record_declaration",
  ],
);
