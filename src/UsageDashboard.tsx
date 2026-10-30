import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { isDesktopApp } from "./settings";

type Query = { from_ms: number; to_ms: number; source: string | null; provider: string | null; model: string | null };
type Config = { schema_version: number; codex_root: string; fabrica_databases: string[] };
type Row = { source: string; provider: string; model: string; actual_usd: number | null; estimate_low_usd: number | null; estimate_high_usd: number | null; elapsed_seconds: number | null; active_seconds: number | null };
type Group = Row & { records: number; actual_sources: string[]; tokens: Record<string, number | null>; events: number; unknown_cost_events: number; unpriced_events: number; cache_write_unknown_events: number; partial_records: number; unknown_active_records: number; pricing: { source: string; verified_on: string } | null };
type Detail = Row & { id: string; state: string; partial: boolean };
type Report = { schema_version: number; query: Query; collection: { at_ms: number | null; errors: string[]; imported_sessions: number; native_databases_found: number }; groups: Group[]; details: Detail[]; detail_count: number; details_truncated: boolean; untimed_records: number; filters: { sources: string[]; providers: string[]; models: string[] } };
const names: Record<string, string> = { codex: "Codex sessions", "fabrica-workflow": "Fabrica workflows", "fabrica-paid": "Fabrica paid calls", "fabrica-task": "Runner task durations" };
const errors: Record<string,string> = { codex_missing:"Codex folder unavailable",codex_empty:"No Codex session files",codex_inventory_limit:"Session inventory limit reached",codex_import_limit:"Import partial; refresh again to continue",codex_read_error:"Some session files could not be read",fabrica_missing:"Some native databases are unavailable",fabrica_read_error:"Some native records could not be read",native_record_limit:"Oversized native record excluded",native_import_limit:"Native import limit reached; coverage is partial",not_collected:"Not collected yet",fork_baseline_unknown:"Fork baseline attribution is incomplete" };
const amount = (value: number | null | undefined) => value == null ? "Unknown" : value === 0 ? "$0.00" : value < .000001 ? "<$0.000001" : `$${value.toFixed(6)}`;
const count = (value: number | null | undefined) => value == null ? "—" : value.toLocaleString();
const duration = (value: number | null | undefined) => value == null ? "Unknown" : `${value.toFixed(1)} s`;
const day = (date: Date) => date.toISOString().slice(0,10);
export function UsageDashboard({ active }: { active: boolean }) {
 const [from,setFrom]=useState(()=>day(new Date(Date.now()-7*86400000)));
 const [to,setTo]=useState(()=>day(new Date(Date.now()+86400000)));
 const [source,setSource]=useState("");const [provider,setProvider]=useState("");const [model,setModel]=useState("");
 const [clock,setClock]=useState(Date.now);
 const [report,setReport]=useState<Report|null>(null);const [busy,setBusy]=useState(false);const [error,setError]=useState("");
 const [config,setConfig]=useState<Config|null>(null);const [databases,setDatabases]=useState("");const [configError,setConfigError]=useState("");
 const serial=useRef(0);const mounted=useRef(true);const inFlight=useRef(false);
 useEffect(()=>{mounted.current=true;return()=>{mounted.current=false;};},[]);
 function query():Query|null {
  const start=Date.parse(`${from}T00:00:00Z`),end=Date.parse(`${to}T00:00:00Z`);
  if (!Number.isFinite(start)||!Number.isFinite(end)||start>=end||end-start>366*86400000){setError("Choose a UTC range of at most 366 days.");return null;}
  return {from_ms:start,to_ms:end,source:source||null,provider:provider||null,model:model||null};
 }
 async function load(refresh:boolean) {
  const selected=query();if (!selected||!isDesktopApp()||inFlight.current)return;inFlight.current=true;
  const request=++serial.current;setBusy(true);setError("");
  try {const value=await invoke<Report>("get_usage_dashboard",{query:selected,refresh});if(mounted.current&&request===serial.current){if(value.schema_version!==1)throw new Error("Usage schema is incompatible.");setReport(value);}}
  catch(e){if(mounted.current&&request===serial.current)setError(String(e));}
  finally{inFlight.current=false;if(mounted.current&&request===serial.current)setBusy(false);}
 }
 useEffect(()=>{if(active&&isDesktopApp()){void load(true);void invoke<Config>("get_usage_dashboard_config").then(v=>{if(mounted.current){setConfig(v);setDatabases(v.fabrica_databases.join("\n"));}}).catch(e=>{if(mounted.current)setConfigError(String(e));});}},[active]);
 async function save(){if(!config)return;setConfigError("");try{const saved=await invoke<Config>("save_usage_dashboard_config",{config:{...config,fabrica_databases:databases.split("\n").map(v=>v.trim()).filter(Boolean)}});setConfig(saved);await load(true);}catch(e){if(mounted.current)setConfigError(String(e));}}
 useEffect(()=>{if(!active)return;const timer=window.setInterval(()=>setClock(Date.now()),60000);return()=>window.clearInterval(timer);},[active]);
 if(!isDesktopApp())return <section className="panel empty-state"><h1>Model Usage</h1><p>Model usage is available in the desktop app.</p></section>;
 const stale=!!report?.collection.at_ms&&clock-report.collection.at_ms>15*60000;
 return <section className="panel usage-dashboard" aria-labelledby="usage-heading" aria-busy={busy}>
  <h1 id="usage-heading">Model Usage</h1><div className="usage-heading"><button type="button" disabled={busy} onClick={()=>void load(true)}>{busy?"Collecting…":"Refresh usage"}</button></div>
  <form className="usage-filters" onSubmit={e=>{e.preventDefault();void load(false);}}>
   <label>From (UTC)<input type="date" disabled={busy} required value={from} onChange={e=>setFrom(e.target.value)}/></label>
   <label>Before (UTC)<input type="date" disabled={busy} required value={to} onChange={e=>setTo(e.target.value)}/></label>
   {([["Source",source,setSource,report?.filters.sources],["Provider",provider,setProvider,report?.filters.providers],["Model",model,setModel,report?.filters.models]] as const).map(([title,value,change,options])=><label key={title}>{title}<select disabled={busy} value={value} onChange={e=>change(e.target.value)}><option value="">All</option>{options?.map(v=><option key={v} value={v}>{names[v]??v}</option>)}</select></label>)}
   <button type="submit" disabled={busy}>Apply filters</button>
  </form>
  <p className="usage-status" role="status">{report?.collection.at_ms?`Collected ${new Date(report.collection.at_ms).toISOString()}${stale?" · Stale":""} · ${report.collection.native_databases_found} native databases`:"Not collected"}</p>
  {error&&<p role="alert">{error}{report?" Showing retained results.":""}</p>}
  {!!report?.collection.errors.length&&<p role="status">{report.collection.errors.map(v=>errors[v]??v).join(" · ")}</p>}
  {report&&!report.groups.length&&<p>No observed usage in this range. Retained history is kept.</p>}
  <div className="usage-cards">{report?.groups.map(g=><article key={`${g.source}:${g.provider}:${g.model}`} className="usage-card">
   <h3>{names[g.source]??g.source}</h3><p className="usage-model">{g.model} · {g.provider}</p>
   <dl><dt>Sessions / runs / calls</dt><dd>{g.records}</dd><dt>Actual USD</dt><dd>{amount(g.actual_usd)}</dd><dt>API-equivalent estimate</dt><dd>{g.estimate_low_usd==null?"Unknown":`${amount(g.estimate_low_usd)} – ${amount(g.estimate_high_usd)}`}</dd><dt>Elapsed span sum</dt><dd>{duration(g.elapsed_seconds)}</dd><dt>Active estimate sum</dt><dd>{duration(g.active_seconds)}</dd></dl>
   <details><summary>Tokens and coverage</summary><dl>{[["Input","input_tokens"],["Cached input (subset)","cached_input_tokens"],["Cache writes (subset)","cache_write_input_tokens"],["Output","output_tokens"],["Reasoning (subset)","reasoning_output_tokens"],["Total tokens","total_tokens"]].map(([title,key])=><div className="usage-token" key={key}><dt>{title}</dt><dd>{count(g.tokens[key])}</dd></div>)}</dl><p>{g.events} usage events · {g.unknown_cost_events} unknown actual costs · {g.unpriced_events} unpriced estimates · {g.cache_write_unknown_events} unknown cache-write counts · {g.partial_records} partial/observed records · {g.unknown_active_records} records without attributed active time.</p>{!!g.actual_sources.length&&<p>Settlement sources: {g.actual_sources.join(", ")}</p>}{g.pricing&&<p>Pricing snapshot {g.pricing.verified_on}: {g.pricing.source}</p>}</details>
   <details><summary>Sessions and runs</summary><div className="usage-table-wrap"><table><caption>{g.model} records</caption><thead><tr><th>ID / state</th><th>Actual USD</th><th>Estimate</th><th>Elapsed</th><th>Active</th></tr></thead><tbody>{report.details.filter(d=>d.source===g.source&&d.provider===g.provider&&d.model===g.model).map(d=><tr key={`${d.source}:${d.id}:${d.model}`}><th scope="row">{d.id}<small>{d.state}{d.partial?" · partial/observed":""}</small></th><td>{amount(d.actual_usd)}</td><td>{d.estimate_low_usd==null?"Unknown":`${amount(d.estimate_low_usd)} – ${amount(d.estimate_high_usd)}`}</td><td>{duration(d.elapsed_seconds)}</td><td>{duration(d.active_seconds)}</td></tr>)}</tbody></table></div></details>
  </article>)}</div>
  {!!report?.details_truncated&&<p role="status">Showing the first {report.details.length} of {report.detail_count} detail rows. Narrow the filters.</p>}
  {!!report?.untimed_records&&<p>{report.untimed_records} untimed records excluded from date-filtered totals.</p>}
  <p className="usage-definition">Active time sums adjacent event gaps of five minutes or less, clipped to the selected UTC interval. Missing timelines stay unknown. Concurrent spans overlap; these sums are not unique wall-clock or human work time.</p>
  <details><summary>Accounting and sources</summary><p>Actual paid-call settlements and API-equivalent estimates have different coverage and are never added. Codex estimates are not subscription bills. Cached input and reasoning are subsets, not extra tokens. Runner task durations are separate from workflow spans; Codex and Fabrica may overlap.</p><p>Usage costs belong to their event timestamp; native call costs belong to dispatch time and are not prorated. Span intervals are clipped to [From, Before). Native paid-call estimates use total-token ranges because input/output/cache splits are unknown. Estimates reconstruct the bundled dated short/long-context rates; missing cache-write counts assume zero, exclude fees and service-tier uplifts, and are not invoice bounds. Known fork copies are deduplicated; fork active time and unmatched inherited token baselines remain unattributed. Missing parent metadata limits fork correlation. Skill loading does not establish per-skill usage.</p>
   {config&&<fieldset disabled={busy}><legend>Verified data locations</legend><label>Codex root<input value={config.codex_root} onChange={e=>setConfig({...config,codex_root:e.target.value})}/></label><label>Additional Fabrica databases (one absolute path per line)<textarea rows={3} value={databases} onChange={e=>setDatabases(e.target.value)}/></label><p>The current native workflow and runner storage locations are always included.</p><button type="button" onClick={()=>void save()}>Save sources and refresh</button></fieldset>}{configError&&<p role="alert">{configError}</p>}
  </details>
 </section>;
}
