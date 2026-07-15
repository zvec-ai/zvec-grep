import type { LanguageAdapter } from "../adapter.js";
import {
  extractCommonModifiers,
  extractGenericSignature,
  extractPrecedingDoc,
} from "../families/metadata.js";

export const RUST_ADAPTER: LanguageAdapter = {
  format: "rust",
  entityTypes: new Set([
    "enum_item",
    "function_item",
    "function_signature_item",
    "impl_item",
    "struct_item",
    "trait_item",
    "type_item",
    "union_item",
  ]),
  scopeTypes: new Set(["impl_item", "mod_item", "trait_item"]),
  extractName(node) {
    if (node.type === "impl_item") {
      return node.childForFieldName("type")?.text;
    }

    return node.childForFieldName("name")?.text;
  },
  extractSignature: extractGenericSignature,
  extractDoc: extractPrecedingDoc,
  extractModifiers: extractCommonModifiers,
};
