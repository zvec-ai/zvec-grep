// Run after the root npm build; expectations come from the actual MCP handler.
import assert from "node:assert/strict";
import { readFile, writeFile } from "node:fs/promises";
import { resolve } from "node:path";
import { Client, InMemoryTransport } from "@modelcontextprotocol/client";
import { createZvecGrepMcpServer } from "../../dist/mcp/tools.js";
const root = resolve("test/fixtures/repository");
const seed = JSON.parse(await readFile(new URL("../compat/mcp/search-presentation.json", import.meta.url))).cases[0].result;
const range = (start, end) => ({kind:"text", start_line:start, end_line:end, start_byte_offset:start*100, end_byte_offset:end*100+1, start_byte_column:0, end_byte_column:1});
const item = (overrides = {}) => ({...structuredClone(seed.items[0]), kind:"lexical_match", rank:1, range:range(10,12), content_range:range(10,12), excerpt_range:range(11,11), content:"before\nneedle\nafter", matched_by:"lexical", metadata:null, container:null, query_groups:[], selection_reason:null, coverage_group:null, ...overrides});
const result = (items, overrides={}) => ({...structuredClone(seed), source:"rg", coverage:"rg_exhaustive", items, diagnostics:{empty_reason:null,index:null,rg:null,structure:null,timings:[]}, ...overrides});
const code = {kind:"code",symbol_type:"function",symbol_name:"lookup",scope:"Store"};
const container = {entity_id:"function-id",range:range(1,20),metadata:code};
const cases = [
  {id:"context",result:result([item()])},
  {id:"long-unbounded",result:result([item({content:Array.from({length:20},(_,i)=>`line-${i}-${"x".repeat(200)}`).join("\n"),range:range(1,20),content_range:range(1,20),excerpt_range:range(12,12)})])},
  {id:"explicit-head",result:result([item()],{coverage:"rg_truncated"})},
  {id:"no-matches",result:result([])},
  {id:"symbol",result:result([item({container})])},
  {id:"symbol-overlap",result:result([item({container}),item({rank:2,container,excerpt_range:range(12,12)})])},
  {id:"declaration",result:result([item({container,content:"function lookup() {}",range:range(1,1),content_range:range(1,1),excerpt_range:range(1,1)})])},
  {id:"files",result:result([item(),item({rank:2,relative_path:"other.txt"})])},
];
const camel = (v) => Array.isArray(v) ? v.map(camel) : v && typeof v === "object" ? Object.fromEntries(Object.entries(v).map(([k,v])=>[k.replace(/_([a-z])/g,(_,c)=>c.toUpperCase()),camel(v)])) : v;
const nodeRange = r => r && ({...camel(r),startOffset:r.start_byte_offset,endOffset:r.end_byte_offset});
for (const fixture of cases) {
 const value=fixture.result;
 const response={result:{...camel(value),root,items:value.items.map(i=>({...camel(i),file:{absolutePath:resolve(root,i.relative_path),relativePath:i.relative_path},range:nodeRange(i.range),excerptRange:nodeRange(i.excerpt_range),...(i.container?{container:{...camel(i.container),range:nodeRange(i.container.range)}}:{})}))}};
 const server=createZvecGrepMcpServer({rg:async()=>response},"fixture",{toolset:"full"});
 const client=new Client({name:"rg-presentation-fixture",version:"1"});
 const [ct,st]=InMemoryTransport.createLinkedPair();
 await Promise.all([client.connect(ct),server.connect(st)]);
 try { const reply=await client.callTool({name:"zvec_grep_rg",arguments:{root,command:"rg needle"}});assert.equal(reply.isError,undefined);fixture.expected=reply.content[0].text; }
 finally { await Promise.all([client.close(),server.close()]); }
}
await writeFile(new URL("../compat/mcp/rg-presentation.json",import.meta.url),`${JSON.stringify({schema_version:1,cases},null,2)}\n`);
console.log(`Captured ${cases.length} public MCP rg cases from Node.js.`);
