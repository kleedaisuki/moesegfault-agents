import RustWorker from '../index.js';
export * from '../index.js';

const state = { armed:false,rttMs:20,rpcs:0,batches:0,businessStatements:0,deferredIngest:0 };
const unavailable = () => { throw Error('Management benchmark must not execute embedding, conversion or Vectorize'); };
class Ai {
  run() { return unavailable(); }
  toMarkdown() { return unavailable(); }
}
function database(target) {
  const original = new WeakMap();
  const sqls = new WeakMap();
  const kickoff = sql => sql.startsWith('SELECT account_id,vector_store_id,file_id,generation,service_profile_json FROM vector_store_files WHERE account_id=');
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
        if(kickoff(sql)) {
          // Isolate the accepted/pending management contract. This fixture
          // defers maintenance discovery identically for both actual Workers.
          state.deferredIngest++;
          return {...await object[key](...args),results:[]};
        }
        if(sql.startsWith('SELECT r.account_id,r.id AS run_id FROM usage_reservations r WHERE'))return object[key](...args);
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
function bindings(env){
  return {...env,DB:database(env.DB),AI:new Ai(),
    VECTORIZE:Object.fromEntries(['query','upsert','deleteByIds','getByIds','describe'].map(name=>[name,unavailable]))};
}
export default class extends RustWorker {
  async fetch(request){
    const path=new URL(request.url).pathname;
    if(path==='/__management/arm'){Object.assign(state,{armed:true,rpcs:0,batches:0,businessStatements:0,deferredIngest:0});return Response.json(state);}
    if(path==='/__management/status')return Response.json(state);
    return RustWorker.prototype.fetch.call({env:bindings(this.env),ctx:this.ctx},request);
  }
}
