import type { LanguageAdapter } from "../adapter.js";
import {
  classifyJavascriptTypescriptNode,
  extractJavascriptTypescriptDoc,
  extractJavascriptTypescriptModifiers,
  extractJavascriptTypescriptName,
  extractJavascriptTypescriptSignature,
  javascriptTypescriptScopeBreadcrumb,
  javascriptTypescriptSourceNode,
  resolveJavascriptTypescriptEntities,
  shouldIndexJavascriptTypescriptEntity,
} from "../families/js-ts.js";

export const JAVASCRIPT_ADAPTER: LanguageAdapter = {
  format: "javascript",
  entityTypes: new Set([
    "assignment_expression",
    "class_declaration",
    "field_definition",
    "function_declaration",
    "generator_function_declaration",
    "method_definition",
    "pair",
    "variable_declarator",
  ]),
  scopeTypes: new Set(["class_declaration"]),
  extractName: extractJavascriptTypescriptName,
  shouldIndexEntity: shouldIndexJavascriptTypescriptEntity,
  resolveEntities: resolveJavascriptTypescriptEntities,
  sourceNode: javascriptTypescriptSourceNode,
  scopeBreadcrumb: javascriptTypescriptScopeBreadcrumb,
  classifyNode: classifyJavascriptTypescriptNode,
  extractSignature: extractJavascriptTypescriptSignature,
  extractDoc: extractJavascriptTypescriptDoc,
  extractModifiers: extractJavascriptTypescriptModifiers,
};
