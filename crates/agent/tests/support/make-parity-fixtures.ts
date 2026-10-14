// Captures the TS reference behavior. Re-run after an intentional TS contract
// change; Rust tests read the committed JSON without Node or SDK dependencies.
import { writeFileSync } from "node:fs";
import { createWorkflowSchema, parseWorkflow } from "../../../../packages/agent/src/workflow/dsl";
import { createStore, substitute } from "../../../../packages/agent/src/workflow/variables";
import { projectTrace, compact, clip, validateNote, renderDraft } from "../../../../packages/agent/src/activity-journal";
import type { Trace, Observation } from "../../../../packages/agent/src/trace-model";
const directory = new URL("../fixtures/", import.meta.url);
const save = (name: string, data: unknown) => writeFileSync(new URL(name, directory), JSON.stringify(data, null, 2)+"\n");
const tools=["t1","t2","set_memory"],skills=["s1","s2"];
const {WorkflowSchema}=createWorkflowSchema({knownTools:tools,knownSkills:skills});
const leaves=[
 {kind:"tool",tool:"t1",args:{},bind:"a"},
 {kind:"llm_compose",preset:"base",skill:"s1",input:{},bind:"a"},
 {kind:"llm_compose",preset:"smart",prompt:"compose",input:{},bind:"a"},
 {kind:"llm_agent",preset:"smart",skill:"s1",prompt:"task",tools:["t1"],maxIterations:2,bind:"a"},
 {kind:"code_agent",task:"calculate",data:"input",bind:"a"},
 {kind:"decide",preset:"filter",items:"${env.items}",prompt:"rules",threshold:0.3,bind:"a"},
];
const cases: Array<{name:string;input:unknown;seeds?:string[]}> = [];
for(const leaf of leaves){
 const base={version:1,steps:[leaf]};cases.push({name:leaf.kind,input:base});
 cases.push({name:leaf.kind+" extra",input:{version:1,steps:[{...leaf,extra:true}]}});
 if("bind" in leaf){cases.push({name:leaf.kind+" empty bind",input:{version:1,steps:[{...leaf,bind:""}]}});cases.push({name:leaf.kind+" null bind",input:{version:1,steps:[{...leaf,bind:null}]}});}
}
for(const [name,input] of Object.entries({
 "tool unknown":{version:1,steps:[{kind:"tool",tool:"hallucinated",args:{}}]},
 "unknown root":{version:1,steps:[{kind:"terminal"}],extra:true},
 "empty workflow":{version:1,steps:[]},"bad version":{version:2,steps:[{kind:"terminal"}]},
 "terminal":{version:1,steps:[{kind:"terminal"}]},"terminal extra":{version:1,steps:[{kind:"terminal",bind:"a"}]},
 "replan":{version:1,steps:[{kind:"replan",context:["a"],note:"gathered"}]},
 "replan empty":{version:1,steps:[{kind:"replan",context:[]}]},
 "parallel":{version:1,steps:[{kind:"parallel",steps:[leaves[0],{...leaves[1],bind:"b"}]}]},
 "parallel single":{version:1,steps:[{kind:"parallel",steps:[leaves[0]]}]},
 "parallel terminal":{version:1,steps:[{kind:"parallel",steps:[leaves[0],{kind:"terminal"}]}]},
 "parallel replan":{version:1,steps:[{kind:"parallel",steps:[leaves[0],{kind:"replan",context:["a"]}]}]},
 "parallel nested":{version:1,steps:[{kind:"parallel",steps:[leaves[0],{kind:"parallel",steps:[leaves[0],leaves[1]]}]}]},
 "compose missing framing":{version:1,steps:[{kind:"llm_compose",preset:"base",input:{},bind:"a"}]},
 "null prompt":{version:1,steps:[{...leaves[1],prompt:null}]},"null data":{version:1,steps:[{...leaves[4],data:null}]},
 "null threshold":{version:1,steps:[{...leaves[5],threshold:null}]},
 "agent too many":{version:1,steps:[{...leaves[3],maxIterations:21}]},
 "agent fractional":{version:1,steps:[{...leaves[3],maxIterations:1.5}]},
 "agent no tools":{version:1,steps:[{...leaves[3],tools:[]}]},
 "agent unknown skill":{version:1,steps:[{...leaves[3],skill:"missing"}]},
 "composer compiler preset":{version:1,steps:[{...leaves[1],preset:"compiler"}]},
 "decide threshold zero":{version:1,steps:[{...leaves[5],threshold:0}]},
 "decide threshold one":{version:1,steps:[{...leaves[5],threshold:1}]},
 "decide unknown preset":{version:1,steps:[{...leaves[5],preset:"rank"}]},
 "unknown kind":{version:1,steps:[{kind:"branch"}]},"null workflow":null,
}))cases.push({name,input});
for(const [name,steps] of Object.entries({
 "ordered bind":[leaves[0],{kind:"tool",tool:"t2",args:{payload:"${a}"},bind:"b"}],
 "missing bind":[{kind:"tool",tool:"t1",args:{payload:"${missing}"},bind:"a"}],
 "same step bind":[{kind:"tool",tool:"t1",args:{payload:"${a}"},bind:"a"}],
 "duplicate bind":[leaves[0],leaves[0]],"reserved bind":[{...leaves[0],bind:"env"}],
 "sibling bind":[{kind:"parallel",steps:[leaves[0],{kind:"tool",tool:"t2",args:{payload:"${a}"},bind:"b"}]}],
 "parallel duplicate":[{kind:"parallel",steps:[leaves[0],leaves[0]]}],
 "after terminal":[{kind:"terminal"},{kind:"tool",tool:"t1",args:{payload:"${missing}"}}],
}))cases.push({name,input:{version:1,steps},seeds:["env","signal"]});
save("workflow-schema.json",{tools,skills,cases:cases.map(c=>({...c,expected:parseWorkflow(c.input,WorkflowSchema,{seeds:c.seeds})}))});
const initial={env:{items:[{title:"a"},null],nothing:null,constructor:"own"},list:[1,{text:"two"}],empty:"",literal:"${missing}"};
const values=["${env}","${list.1.text}","${list.length}","prefix ${env.nothing}","${env.constructor}","${env.items.map}","${env.toString}","${list.01}","${missing}","${empty}",{x:["${list.0}","${env.items.1}","${literal}"]}];
save("workflow-variables.json",{initial,cases:values.map(value=>{try{return {value,ok:true,result:substitute(value,createStore(initial))};}catch(error){return {value,ok:false,error:error instanceof Error?error.message:String(error)};}})});
const observation=(id:string,type:Observation["type"],parent:string|null,metadata:Record<string,unknown>,input:unknown,output:unknown,level="DEFAULT"):Observation=>({id,name:"arbitrary-name",type,parentObservationId:parent,startTime:"2026-10-01T00:00:00.000Z",endTime:"2026-10-01T00:00:01.000Z",level,statusMessage:null,input,output,metadata,model:null,modelParameters:null,usage:null,usageDetails:null,calculatedTotalCost:null,latency:1});
const trace:Trace={id:"run",name:"signal:scheduler",sessionId:"scheduler:42",timestamp:"2026-10-01T00:00:00.000Z",input:"давай",output:{ok:true,stepCount:4},metadata:{activity:"run",skills:["scheduler","routing"]},observations:[],latency:1,totalCost:0,tags:["scheduler","planner-mode"]};
const observations=[observation("root","AGENT",null,{},null,null),observation("primary","CHAIN","root",{activity:"primary_loop"},"recent discussion\nдавай","finished"),observation("auto","TOOL","root",{tool:"get_telegram_chat_history",automatic:true},{},"omitted"),observation("noise","TOOL","root",{tool:"working_memory_get"},{},"omitted"),observation("worker","AGENT","root",{skill:"news-digest"},{prompt:"make digest"},"result "+"🦊".repeat(500)),...Array.from({length:12},(_,i)=>observation("read"+i,"TOOL","worker",{tool:"fetch_article"},{url:"https://example.test/"+i},JSON.stringify({title:"Article",body:"ARTICLE_BODY_".repeat(1000)}))),observation("send","TOOL","worker",{tool:"send_telegram_message"},{chatId:"42",text:"Delivered summary"},'{"message_id":10}'),observation("partial","TOOL","root",{tool:"send_telegram_message"},{chatId:"42",text:"body"},"[tool error] delivered parts 1/2, then failed","ERROR"),observation("retry1","EVENT","worker",{attempt:1},null,null,"WARNING"),observation("retry2","EVENT","worker",{attempt:2},null,null,"WARNING")];
const projectionCases=[{trace,observations},{trace:{...trace,metadata:{error:true},output:{error:"failure"}},observations},{trace:{...trace,input:"x".repeat(50000),output:"done"},observations}, {trace,observations:observations.map(o=>({...o,name:"RENAMED"}))}];
save("activity-projection.json",{cases:projectionCases.map(c=>({...c,expected:projectTrace(c.trace,c.observations,"2026-10-01 00:00:01")})),compact:compact({a:Array.from({length:20},()=>({body:"BODY".repeat(100)})),x:"🦊".repeat(100)}, {strChars:50,dropBodies:true,depth:3}),clip:clip("🦊".repeat(20),11),notes:["","line\n\nsecond  ","```bad```","x".repeat(1501),Array.from({length:11},()=>"line").join("\n")].map(raw=>({raw,expected:validateNote(raw)})),draft:renderDraft(projectTrace(trace,observations,"2026-10-01 00:00:01"),"note")});
