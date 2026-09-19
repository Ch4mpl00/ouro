#!/usr/bin/env node
// Static lint for the exported n8n workflows in ../workflows.
//
// n8n itself only complains after an import, in the editor, one node at a time;
// most of what breaks a hand-edited export is mechanical and checkable here:
// a connection naming a node that was renamed, a `$('Node')` reference that no
// longer resolves, an Execute Workflow node pointing at an id no file defines,
// or a syntax error inside a Code node's jsCode.
//
//   node n8n/scripts/validate-workflows.mjs

import { readdirSync, readFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const workflowsDir = resolve(dirname(fileURLToPath(import.meta.url)), "..", "workflows");

const TRIGGER_TYPES = new Set([
  "n8n-nodes-base.scheduleTrigger",
  "n8n-nodes-base.executeWorkflowTrigger",
  "n8n-nodes-base.errorTrigger",
  "n8n-nodes-base.manualTrigger",
  "n8n-nodes-base.webhook",
]);

// Sub-nodes hang off a parent node through a typed connection (ai_languageModel,
// ai_outputParser, …) instead of the main flow.
// Sticky notes are canvas documentation — never connected to anything.
const STANDALONE_TYPES = new Set(["n8n-nodes-base.stickyNote"]);

const SUB_NODE_PREFIXES = ["@n8n/n8n-nodes-langchain.lmChat", "@n8n/n8n-nodes-langchain.outputParser"];

const problems = [];
const warnings = [];

const files = readdirSync(workflowsDir).filter((name) => name.endsWith(".json")).sort();
if (files.length === 0) {
  console.error(`no workflow json found in ${workflowsDir}`);
  process.exit(1);
}

const workflows = files.map((file) => {
  const raw = readFileSync(join(workflowsDir, file), "utf8");
  try {
    return { file, wf: JSON.parse(raw), raw };
  } catch (err) {
    problems.push(`${file}: not valid JSON — ${err.message}`);
    return null;
  }
}).filter(Boolean);

const knownWorkflowIds = new Set(workflows.map(({ wf }) => wf.id));

// Every string in the workflow, with a path for the error message.
function* strings(value, path = "") {
  if (typeof value === "string") {
    yield [path, value];
  } else if (Array.isArray(value)) {
    for (const [i, entry] of value.entries()) yield* strings(entry, `${path}[${i}]`);
  } else if (value && typeof value === "object") {
    for (const [key, entry] of Object.entries(value)) yield* strings(entry, path ? `${path}.${key}` : key);
  }
}

for (const { file, wf } of workflows) {
  const where = (msg) => `${file}: ${msg}`;

  if (!wf.id) problems.push(where("workflow has no id — Execute Workflow nodes elsewhere cannot reference it"));
  if (!wf.name) problems.push(where("workflow has no name"));
  if (!Array.isArray(wf.nodes) || wf.nodes.length === 0) {
    problems.push(where("workflow has no nodes"));
    continue;
  }

  const names = new Set();
  const ids = new Set();
  for (const node of wf.nodes) {
    if (!node.name) problems.push(where("a node has no name"));
    if (names.has(node.name)) problems.push(where(`duplicate node name "${node.name}"`));
    names.add(node.name);
    if (node.id) {
      if (ids.has(node.id)) problems.push(where(`duplicate node id "${node.id}" (${node.name})`));
      ids.add(node.id);
    }
    if (!node.type) problems.push(where(`node "${node.name}" has no type`));
    if (node.typeVersion === undefined) problems.push(where(`node "${node.name}" has no typeVersion`));
    if (!Array.isArray(node.position) || node.position.length !== 2) {
      problems.push(where(`node "${node.name}" has no [x, y] position`));
    }

    // Code nodes: parse the JS so a typo fails here and not at 21:00.
    if (node.type === "n8n-nodes-base.code") {
      const code = node.parameters?.jsCode;
      if (typeof code !== "string" || code.trim() === "") {
        problems.push(where(`code node "${node.name}" has no jsCode`));
      } else {
        try {
          // Same wrapping n8n uses: the code is a function body, and `return` at
          // the top level is legal.
          new Function(code);
        } catch (err) {
          problems.push(where(`code node "${node.name}" has a syntax error — ${err.message}`));
        }
      }
    }

    // Execute Workflow nodes must point at a workflow this repo defines.
    if (node.type === "n8n-nodes-base.executeWorkflow") {
      const target = node.parameters?.workflowId?.value;
      if (!target) problems.push(where(`"${node.name}" has no workflowId`));
      else if (!knownWorkflowIds.has(target)) {
        problems.push(where(`"${node.name}" calls workflow id "${target}", which no file in workflows/ defines`));
      }
    }

    if (node.credentials) {
      for (const [type, cred] of Object.entries(node.credentials)) {
        if (typeof cred?.id === "string" && cred.id.startsWith("REPLACE_WITH")) {
          warnings.push(where(`"${node.name}" still has the placeholder ${type} credential — re-select it after import`));
        }
      }
    }
  }

  // Connections: both ends must exist, and the source must be a real node.
  const connections = wf.connections ?? {};
  const hasInbound = new Set();
  for (const [source, outputs] of Object.entries(connections)) {
    if (!names.has(source)) problems.push(where(`connections reference unknown source node "${source}"`));
    for (const [outputType, branches] of Object.entries(outputs)) {
      for (const branch of branches ?? []) {
        for (const link of branch ?? []) {
          if (!names.has(link.node)) {
            problems.push(where(`connection ${source} -[${outputType}]-> "${link.node}" targets an unknown node`));
          }
          hasInbound.add(link.node);
        }
      }
    }
  }

  for (const node of wf.nodes) {
    const isTrigger = TRIGGER_TYPES.has(node.type);
    const isSubNode = SUB_NODE_PREFIXES.some((prefix) => node.type?.startsWith(prefix));
    if (isTrigger || isSubNode || STANDALONE_TYPES.has(node.type)) continue;
    if (!hasInbound.has(node.name)) {
      problems.push(where(`node "${node.name}" is unreachable — nothing connects into it`));
    }
  }

  // $('Node name') references, in expressions and in Code nodes alike.
  for (const [path, value] of strings(wf)) {
    for (const match of value.matchAll(/\$\(\s*['"]([^'"]+)['"]\s*\)/g)) {
      if (!names.has(match[1])) {
        problems.push(where(`${path} references node $('${match[1]}'), which does not exist`));
      }
    }
  }

  const errorWorkflow = wf.settings?.errorWorkflow;
  if (errorWorkflow && !knownWorkflowIds.has(errorWorkflow)) {
    problems.push(where(`settings.errorWorkflow "${errorWorkflow}" is not defined by any file in workflows/`));
  }
}

for (const warning of warnings) console.log(`warn  ${warning}`);
for (const problem of problems) console.error(`error ${problem}`);

console.log(
  `\nchecked ${workflows.length} workflow(s): ${problems.length} error(s), ${warnings.length} warning(s)`,
);
process.exit(problems.length > 0 ? 1 : 0);
