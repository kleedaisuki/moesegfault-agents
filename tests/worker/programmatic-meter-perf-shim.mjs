import RustWorker, {Session as RustSession} from '../index.js';
export * from '../index.js';

const state = {armed:false,rttMs:20,rpcs:0,batches:0,businessStatements:0};
function database(target) {
  const original = new WeakMap();
  const sqls = new WeakMap();
  const count = async sources => {
    if (!state.armed) return;
    state.rpcs++;
    state.batches += Number(sources.length>1);
    state.businessStatements += sources.filter(sql => !/resource_observations|usage_ledger|usage_reservations/.test(sql)).length;
    await new Promise(resolve => setTimeout(resolve,state.rttMs));
  };
  const statement = (target,sql) => {
    const wrapped = new Proxy(target,{get(object,key){
      if(key==='constructor')return object.constructor;
      if(key==='bind')return (...args)=>statement(object.bind(...args),sql);
      if(['run','all','first','raw'].includes(key))return async (...args)=>{
        await count([sql]);
        return object[key](...args);
      };
      const result=Reflect.get(object,key,object);
      return typeof result==='function'?result.bind(object):result;
    }});
    original.set(wrapped,target);sqls.set(wrapped,sql);return wrapped;
  };
  return new Proxy(target,{get(object,key){
    if(key==='constructor')return object.constructor;
    if(key==='prepare')return sql=>statement(object.prepare(sql),sql);
    if(key==='batch')return async statements=>{
      await count(statements.map(item=>sqls.get(item)??''));
      return object.batch(statements.map(item=>original.get(item)??item));
    };
    const result=Reflect.get(object,key,object);
    return typeof result==='function'?result.bind(object):result;
  }});
}
function bindings(env){ return {...env,DB:database(env.DB)}; }
export class Session extends RustSession { constructor(ctx,env){super(ctx,bindings(env));} }
export default class extends RustWorker {
  async fetch(request){
    const path=new URL(request.url).pathname;
    if(path==='/__meter/arm'){Object.assign(state,{armed:true,rpcs:0,batches:0,businessStatements:0});return Response.json(state);}
    if(path==='/__meter/status')return Response.json(state);
    return RustWorker.prototype.fetch.call({env:bindings(this.env),ctx:this.ctx},request);
  }
}
