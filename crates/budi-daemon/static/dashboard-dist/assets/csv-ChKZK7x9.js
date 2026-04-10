import{m as d,D as l}from"./state-mLsTektW.js";import{j as n,c as a}from"./index-Bx8ukwah.js";/**
 * @license lucide-react v0.548.0 - ISC
 *
 * This source code is licensed under the ISC license.
 * See the LICENSE file in the root directory of this source tree.
 */const u=[["path",{d:"M12 5v14",key:"s699le"}],["path",{d:"m19 12-7 7-7-7",key:"1idqje"}]],f=d("arrow-down",u);/**
 * @license lucide-react v0.548.0 - ISC
 *
 * This source code is licensed under the ISC license.
 * See the LICENSE file in the root directory of this source tree.
 */const p=[["path",{d:"m5 12 7-7 7 7",key:"hav0vg"}],["path",{d:"M12 19V5",key:"x0mq9r"}]],x=d("arrow-up",p);/**
 * @license lucide-react v0.548.0 - ISC
 *
 * This source code is licensed under the ISC license.
 * See the LICENSE file in the root directory of this source tree.
 */const b=[["path",{d:"M12 15V3",key:"m9g1x1"}],["path",{d:"M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4",key:"ih7n3h"}],["path",{d:"m7 10 5 5 5-5",key:"brsn70"}]],v=d("download",b);function y({className:t,...e}){return n.jsx("table",{className:a("w-full caption-bottom text-sm",t),...e})}function g({className:t,...e}){return n.jsx("thead",{className:a("border-b border-border [&_tr]:border-b-0 [&_tr]:hover:bg-transparent",t),...e})}function j({className:t,...e}){return n.jsx("tbody",{className:a("[&_tr:last-child]:border-0",t),...e})}function w({className:t,...e}){return n.jsx("tr",{className:a("border-b border-border/70 transition hover:bg-muted/40",t),...e})}function k({className:t,...e}){return n.jsx("th",{className:a("h-10 px-3 text-left align-middle text-xs font-semibold uppercase tracking-wide text-muted-foreground transition-colors hover:bg-primary/15 hover:text-foreground",t),...e})}function _({className:t,...e}){return n.jsx("td",{className:a("p-3 align-middle",t),...e})}function c(t){if(t==null)return"";const e=String(t);return/[",\n\r]/.test(e)?`"${e.replace(/"/g,'""')}"`:e}function N(t,e){const s=e.map(o=>c(o.header)).join(","),r=t.map(o=>e.map(i=>c(i.value(o))).join(","));return[s,...r].join(`
`)}function C(t,e){const s=new Blob([t],{type:"text/csv;charset=utf-8;"}),r=URL.createObjectURL(s),o=document.createElement("a");o.href=r,o.download=e,o.style.display="none",document.body.appendChild(o),o.click(),document.body.removeChild(o),URL.revokeObjectURL(r)}function L(t,e){const s=new Date().toISOString().slice(0,10),r=[t];if(e){const o=l(e.period).toLowerCase().replace(/\s+/g,"-");r.push(o)}return r.push(s),`${r.join("_")}.csv`}export{x as A,v as D,y as T,g as a,w as b,k as c,C as d,j as e,_ as f,f as g,L as h,N as t};
