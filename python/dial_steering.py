"""Bounded, model-bound MLX activation steering. JSON tensors only; never pickle or code.

Inference profiles are fixed for a server lifetime. Worker contexts always restore original layers.
Fingerprints bind local model/adapter metadata; they are not full weight-content attestations.
"""
import contextlib
import hashlib
import json
import math
import os
from pathlib import Path
import time

VERSION = 1
MAX_BYTES = 4 * 1024 * 1024
STATUS = {"state": "disabled", "uses": 0}


class SteeringError(ValueError):
    """Static operator-facing contract failure; messages never contain prompt text."""


def require(condition, message):
    if not condition:
        raise SteeringError(message)


def read_json(path):
    with Path(path).open('rb') as source:
        raw = source.read(MAX_BYTES + 1)
    require(len(raw) <= MAX_BYTES, 'steering file exceeds 4 MiB')
    return json.loads(raw)


def digest(value):
    return hashlib.sha256(json.dumps(value, sort_keys=True, separators=(',', ':'), allow_nan=False).encode()).hexdigest()


def identity(directory):
    if directory is None:
        return None
    root = Path(directory).resolve(strict=True)
    require(root.is_dir(), 'model/adapter must be a local directory')
    files = sorted(p for p in root.iterdir() if p.suffix in ('.json', '.safetensors'))
    require(0 < len(files) <= 256, 'model metadata exceeds fingerprint budget')
    records = []
    for path in files:
        stat = path.stat()
        require(path.is_file(), 'invalid model metadata')
        header = None
        if path.suffix == '.json' and path.name in ('config.json', 'adapter_config.json', 'tokenizer_config.json'):
            header = digest(read_json(path))
        records.append([path.name, stat.st_size, stat.st_mtime_ns, header])
    return digest([str(root), records])


def payload(bundle):
    return {key: bundle[key] for key in ('version', 'model_name', 'model', 'adapter', 'layer', 'layer_count', 'dimension', 'vector', 'training_prompts', 'pairs')}


def validate(bundle, strength=None, admitted=False):
    require(isinstance(bundle, dict) and bundle.get('version') == VERSION, 'unsupported steering format')
    require(type(bundle.get('layer')) is int and type(bundle.get('layer_count')) is int and 0 <= bundle['layer'] < bundle['layer_count'] <= 256, 'invalid steering layer')
    size = bundle.get('dimension')
    require(type(size) is int and 1 <= size <= 32768, 'invalid hidden dimension')
    vector = bundle.get('vector')
    require(isinstance(vector, list) and len(vector) == size and all(type(x) in (float, int) and math.isfinite(x) for x in vector), 'invalid steering tensor')
    norm = sum(x*x for x in vector)
    require(0.99 <= norm <= 1.01, 'steering vector must have unit L2 norm')
    require(type(bundle.get('pairs')) is int and 8 <= bundle['pairs'] <= 64, 'at least eight matched extraction pairs required')
    hashes=bundle.get('training_prompts')
    require(isinstance(hashes,list) and len(hashes)==bundle['pairs'] and all(isinstance(h,str) and len(h)==64 and all(c in '0123456789abcdef' for c in h) for h in hashes), 'invalid extraction provenance')
    require(isinstance(bundle.get('model'), str) and len(bundle['model'])==64 and (bundle.get('adapter') is None or isinstance(bundle['adapter'],str) and len(bundle['adapter'])==64), 'invalid model binding')
    require(bundle.get('id') == digest(payload(bundle)), 'steering artifact fingerprint mismatch')
    if strength is not None:
        require(type(strength) in (float,int) and math.isfinite(strength) and 0 < strength <= 2, 'strength must be finite in (0, 2]')
    if admitted:
        evaluation=bundle.get('evaluation',{})
        require(evaluation.get('passed') is True and evaluation.get('artifact')==bundle['id'] and evaluation.get('strength')==strength, 'steering needs a passing evaluation at this exact strength')
        require(type(evaluation.get('cases')) is int and 8<=evaluation['cases']<=32 and evaluation.get('scope')=='held-out response contracts', 'invalid steering evaluation')
        base=evaluation.get('baseline_passes');steered=evaluation.get('steered_passes')
        require(type(base)is int and type(steered)is int and 0<=base<steered<=evaluation['cases'] and steered/evaluation['cases']>=0.8, 'steering evaluation did not improve held-out contracts')
        require(all(evaluation.get(key)is True for key in ('tool_nonregression','repetition_nonregression','latency_nonregression')), 'steering regression gate failed')
    return bundle


def bind(bundle, model_path, adapter_path):
    if not Path(model_path).is_dir():
        from mlx_lm.utils import _download
        model_path=str(_download(model_path).resolve(strict=True))
    require(bundle['model']==identity(model_path) and bundle['adapter']==identity(adapter_path), 'model or adapter fingerprint changed')


def layer_list(model):
    # Some multimodal models expose vision layers on the outer object. Always target the
    # language decoder so image encoding remains unchanged and worker/server layer indices agree.
    decoder=getattr(model,'language_model',model)
    layers=getattr(decoder,'layers',None)
    if layers is None:
        layers=getattr(getattr(decoder,'model',None),'layers',None)
    require(isinstance(layers,list) and 1<=len(layers)<=256, 'architecture does not expose mutable transformer layers')
    return layers


def text_model(model):
    """Expose a VLM decoder's logits/cache through MLX-LM's text evaluation interface."""
    if not hasattr(model,'language_model'):
        return model
    import mlx.nn as nn
    class Decoder(nn.Module):
        def __init__(self):
            super().__init__()
            self['inner']=model.language_model
        def __getattr__(self,name):
            try:
                return super().__getattr__(name)
            except AttributeError:
                if 'inner' not in self:raise
                return getattr(self['inner'],name)
        def __call__(self,*args,**kwargs):
            output=self.inner(*args,**kwargs)
            return getattr(output,'logits',output)
    return Decoder()


def wrap(layer, callback):
    import mlx.nn as nn
    class Wrapped(nn.Module):
        def __init__(self):
            super().__init__()
            self['inner']=layer
        def __getattr__(self,name):
            try:
                return super().__getattr__(name)
            except AttributeError:
                if 'inner' not in self:raise
                return getattr(self['inner'],name)
        def __call__(self,*args,**kwargs):
            result=self.inner(*args,**kwargs)
            return callback(result)
    return Wrapped()


@contextlib.contextmanager
def hook(model, index, callback):
    layers=layer_list(model)
    require(type(index) is int and 0<=index<len(layers), 'layer is out of range')
    original=layers[index]
    layers[index]=wrap(original,callback)
    try:
        yield
    finally:
        layers[index]=original


def hidden(output):
    import mlx.core as mx
    require(isinstance(output,mx.array) and output.ndim==3 and output.shape[1]>0, 'unsupported transformer output: expected [batch, sequence, hidden] array')
    return output


def steering_callback(vector, strength, counted=False):
    import mlx.core as mx
    direction=mx.array(vector,dtype=mx.float32)
    def add(output):
        output=hidden(output)
        require(output.shape[-1]==direction.shape[0], 'hidden dimension changed')
        if counted:
            STATUS['uses']+=1
            STATUS['state']='used'
        return output + (direction*strength).astype(output.dtype)
    return add


def apply_server_vector(model, model_path, adapter_path, specification, engine):
    """Admission and shape probing happen before the first cache or decoding request is built."""
    STATUS.clear();STATUS.update(state='bypassed',uses=0,engine=engine)
    try:
        bundle=validate(read_json(specification['path']),specification['strength'],admitted=True)
        require(bundle['id']==specification['id'], 'steering file changed since admission')
        bind(bundle,model_path,adapter_path)
        layers=layer_list(model)
        require(len(layers)==bundle['layer_count'], 'layer count changed')
        import mlx.core as mx
        captured=[]
        with hook(model,bundle['layer'],lambda output:(captured.append(hidden(output).shape[-1]) or output)):
            mx.eval(text_model(model)(mx.array([[0]],dtype=mx.int32)))
        require(captured==[bundle['dimension']], 'architecture hidden dimension does not match vector')
        layers[bundle['layer']]=wrap(layers[bundle['layer']],steering_callback(bundle['vector'],specification['strength'],True))
        STATUS.update(state='loaded',id=bundle['id'],layer=bundle['layer'],strength=specification['strength'],uses=0,scope='fixed server profile; cache isolated by server lifetime')
    except Exception as error:
        STATUS.update(state='bypassed',reason=str(error),uses=0)


def install_vision_server(server, specification):
    """Attach to MLX-VLM's GPU-thread resource loader, before batching or fallback generation."""
    import importlib
    require(callable(getattr(getattr(server,'app',None),'add_api_route',None)),
            'steering requires a compatible MLX-VLM FastAPI server')
    generation=importlib.import_module('mlx_vlm.server.generation')
    original=getattr(generation,'load_model_resources',None)
    require(callable(original),'steering requires MLX-VLM load_model_resources')
    STATUS.clear();STATUS.update(state='disabled',uses=0,engine='mlx-vlm')
    if specification is not None:
        def load(model_path,adapter_path):
            model,processor,config=original(model_path,adapter_path)
            # Dial launches a fixed profile with APC disabled. Reject a manually injected draft
            # profile rather than sharing a steered target's caches with an unsteered draft.
            runtime=getattr(server,'runtime',None)
            draft=getattr(getattr(runtime,'config',None),'spec_draft_model',None)
            if draft or os.environ.get('MLX_VLM_DRAFT_MODEL'):
                STATUS.clear();STATUS.update(state='bypassed',uses=0,engine='mlx-vlm',reason='steering does not support speculative inference')
            else:
                apply_server_vector(model,model_path,adapter_path,specification,'mlx-vlm')
            return model,processor,config
        generation.load_model_resources=load
    # Return a snapshot; the GPU thread updates confirmed use while FastAPI serves this endpoint.
    server.app.add_api_route('/dial/steering',lambda:dict(STATUS),methods=['GET'])


def install_server(server, specification):
    """Patch compatible MLX-LM APIs; other backends may run without steering."""
    provider_class=getattr(server,'ModelProvider',None)
    handler_class=getattr(server,'APIHandler',None)
    original=getattr(provider_class,'_load',None)
    get=getattr(handler_class,'do_GET',None)
    if not callable(original) or not callable(get):
        require(specification is None, 'steering requires a compatible MLX-LM server with ModelProvider._load and APIHandler.do_GET; disable steering for this backend')
        return
    def load(provider, model_path, adapter_path=None, draft_model_path=None):
        original(provider,model_path,adapter_path,draft_model_path)
        if provider.is_distributed or provider.draft_model is not None:
            STATUS.clear();STATUS.update(state='bypassed',uses=0,reason='steering does not support distributed or speculative inference')
        else:
            apply_server_vector(provider.model,model_path,adapter_path,specification,'mlx-lm')
    if specification is not None:
        provider_class._load=load
    def do_get(handler):
        if handler.path=='/dial/steering':
            raw=json.dumps(STATUS,allow_nan=False).encode();handler.send_response(200);handler.send_header('Content-Type','application/json');handler.send_header('Content-Length',str(len(raw)));handler.end_headers();handler.wfile.write(raw)
        else:
            get(handler)
    handler_class.do_GET=do_get


def atomic_json(path, value):
    path=Path(path);path.parent.mkdir(parents=True,exist_ok=True,mode=0o700)
    raw=json.dumps(value,allow_nan=False).encode();require(len(raw)<=MAX_BYTES,'steering result exceeds 4 MiB')
    import tempfile
    fd,name=tempfile.mkstemp(prefix='.steering-',dir=path.parent)
    try:
        with os.fdopen(fd,'wb') as output:output.write(raw);output.flush();os.fsync(output.fileno())
        os.replace(name,path)
    finally:
        if os.path.exists(name):os.unlink(name)


def messages(value, assistant=False):
    require(isinstance(value,list) and 1<=len(value)<=8,'messages must contain 1–8 entries')
    from dial_worker import messages as validate_messages
    validate_messages(value, training=assistant)
    require(len(json.dumps(value).encode())<=16000,'steering messages exceed byte budget')
    return value


def prompt(tokenizer, value, complete=False, tools=None):
    kwargs={'add_generation_prompt':not complete,'tokenize':True}
    if tools:kwargs['tools']=tools
    tokens=tokenizer.apply_chat_template(value,**kwargs)
    require(0<len(tokens)<=2048,'steering prompt exceeds 2048-token budget')
    return tokens


def load(request):
    model_path=request['model'];adapter=request.get('adapter')
    require(Path(model_path).is_dir(),'steering requires a resolved local model directory')
    config=read_json(Path(model_path)/'config.json')
    if isinstance(config.get('vision_config'),dict):
        from mlx_vlm import load as vlm_load
        from mlx_lm.tokenizer_utils import load as load_tokenizer
        model,processor=vlm_load(model_path,adapter_path=adapter,trust_remote_code=False)
        # Evaluation contracts are text/tool tasks, using the same decoder weights as image
        # inference. Load the model's tokenizer with its EOS and tool markers for exact checks.
        eos=getattr(getattr(processor,'stopping_criteria',None),'eos_token_ids',None)
        if eos is None:
            eos=getattr(getattr(getattr(processor,'tokenizer',None),'stopping_criteria',None),'eos_token_ids',None)
        tokenizer=load_tokenizer(Path(model_path),{'trust_remote_code':False},eos_token_ids=eos)
        return text_model(model),tokenizer
    from mlx_lm import load as mlx_load
    return mlx_load(model_path,adapter_path=adapter,tokenizer_config={'trust_remote_code':False})


def progress(request, state):
    atomic_json(request['progress'],state)


def resume(request, fingerprint, initial):
    try:
        previous=read_json(request['progress'])
        if previous.get('fingerprint')==fingerprint and all(key in previous for key in initial):
            require(type(previous['completed'])is int and 0<=previous['completed']<=initial['total'] and previous['total']==initial['total'], 'corrupt steering checkpoint')
            if 'sum' in initial:
                values=previous['sum']
                require((previous['completed']==0 and values is None) or isinstance(values,list) and 1<=len(values)<=32768 and all(type(x)in (float,int) and math.isfinite(x) for x in values), 'corrupt activation checkpoint')
            if 'results' in initial:
                require(isinstance(previous['results'],list) and len(previous['results'])==previous['completed'], 'corrupt evaluation checkpoint')
                for row in previous['results']:
                    for condition in ('baseline','steered'):
                        result=row[condition]
                        require(type(result['passed'])is bool and type(result['tool_valid'])is bool and type(result['first_token_secs'])in(float,int) and math.isfinite(result['first_token_secs']) and result['first_token_secs']>=0 and type(result['repeat_ratio'])in(float,int) and 0<=result['repeat_ratio']<=1, 'corrupt response metrics')
            previous['phase']=initial['phase']
            return previous
    except (OSError,ValueError,KeyError,TypeError):pass
    return dict(initial,fingerprint=fingerprint)


def extract(request):
    import mlx.core as mx
    pairs=request['pairs'];require(isinstance(pairs,list) and 8<=len(pairs)<=64,'extraction requires 8–64 matched pairs')
    train=[]
    for pair in pairs:
        positive=messages(pair['positive'],True);negative=messages(pair['negative'],True)
        require(positive[:-1]==negative[:-1] and positive[-1]!=negative[-1], 'contrast pairs must share a prompt and differ only in the assistant response')
        train.append(digest(positive[:-1]))
    require(len(set(train))==len(train),'contrast prompts must be distinct')
    binding={'model':identity(request['model']),'adapter':identity(request.get('adapter'))}
    fingerprint=digest([pairs,binding,request['layer'],VERSION])
    state=resume(request,fingerprint,{'phase':'extracting','completed':0,'total':len(pairs),'sum':None})
    progress(request,state)
    model,tokenizer=load(request);layers=layer_list(model);index=request['layer'] if request['layer'] is not None else len(layers)//2;require(type(index)is int and 0<=index<len(layers),'layer out of range')
    total=mx.array(state['sum'],dtype=mx.float32) if state['sum'] is not None else None
    for pair in pairs[state['completed']:]:
        activations=[]
        def capture(output):
            output=hidden(output);activations.append(mx.mean(output[:,-1,:].astype(mx.float32),axis=0));return output
        for key in ('positive','negative'):
            with hook(model,index,capture):
                logits=model(mx.array([prompt(tokenizer,pair[key],True,tools=pair.get('tools'))],dtype=mx.int32))
                mx.eval(logits,*activations)
        require(len(activations)==2,'layer did not run exactly once per contrast')
        difference=activations[0]-activations[1];total=difference if total is None else total+difference
        mx.eval(total)
        state.update(completed=state['completed']+1,sum=total.tolist())
        progress(request,state)
        mx.clear_cache()
    vector=total/len(pairs);norm=float(mx.sqrt(mx.sum(vector*vector)).item());require(math.isfinite(norm) and norm>1e-6,'contrast did not produce a finite direction')
    vector=(vector/norm).tolist();bundle=dict(version=VERSION,model_name=request['model_name'],**binding,layer=index,layer_count=len(layers),dimension=len(vector),vector=vector,training_prompts=train,pairs=len(pairs));bundle['id']=digest(payload(bundle));validate(bundle)
    atomic_json(request['output'],bundle);progress(request,dict(fingerprint=fingerprint,phase='awaiting evaluation',completed=len(pairs),total=len(pairs)))
    return {'state':'awaiting evaluation','id':bundle['id'],'dimension':len(vector),'layer':index,'pairs':len(pairs)}


def parse_tool(text, tokenizer):
    start=getattr(tokenizer,'tool_call_start',None);end=getattr(tokenizer,'tool_call_end',None)
    body=text
    if start and start in text:
        body=text.split(start,1)[1]
        if end and end in body:body=body.split(end,1)[0]
        elif end:return None
    try:
        parsed=json.loads(body.strip())
        require(isinstance(parsed,dict) and isinstance(parsed.get('name'),str) and isinstance(parsed.get('arguments'),dict),'invalid tool call')
        return parsed
    except (ValueError,TypeError):return None


def check_case(case,text,tokenizer):
    required=case.get('required',[]);forbidden=case.get('forbidden',[])
    passed=all(term in text for term in required) and all(term not in text for term in forbidden)
    tool_ok=True
    if case.get('expected_tool'):
        parsed=parse_tool(text,tokenizer);tool_ok=bool(parsed and parsed['name']==case['expected_tool'] and all(key in parsed['arguments'] for key in case.get('required_arguments',[])) and all(parsed['arguments'].get(key)==value for key,value in case.get('expected_arguments',{}).items()))
        passed &= tool_ok
    if case.get('python_ast'):
        import ast
        source=text.strip()
        if source.startswith('```') and source.endswith('```'):source='\n'.join(source.splitlines()[1:-1])
        try:ast.parse(source)
        except (SyntaxError,ValueError):passed=False
    return bool(passed),tool_ok


def generate_case(model,tokenizer,case,limit):
    import mlx.core as mx
    from mlx_lm import stream_generate
    from mlx_lm.sample_utils import make_sampler
    tokens=prompt(tokenizer,case['messages'],tools=case.get('tools'));mx.random.seed(42)
    begin=time.monotonic();first=None;text='';count=0;tool_complete=None
    for item in stream_generate(model,tokenizer,prompt=tokens,max_tokens=limit,sampler=make_sampler(temp=0.0)):
        if first is None:first=time.monotonic()-begin
        text+=item.text;count+=1
        if case.get('expected_tool') and tool_complete is None and parse_tool(text,tokenizer):tool_complete=time.monotonic()-begin
    passed,tool_ok=check_case(case,text,tokenizer)
    words=text.split();repeated=1-len(set(tuple(words[i:i+4]) for i in range(max(0,len(words)-3))))/max(1,len(words)-3) if len(words)>=4 else 0.0
    return {'passed':passed,'tool_valid':tool_ok,'first_token_secs':first or time.monotonic()-begin,'tool_call_complete_secs':tool_complete,'seconds':time.monotonic()-begin,'tokens':count,'repeat_ratio':repeated}


def evaluate(request):
    import mlx.core as mx
    bundle=validate(read_json(request['output']),request['strength']);bind(bundle,request['model'],request.get('adapter'))
    cases=request['cases'];require(isinstance(cases,list) and 8<=len(cases)<=32,'evaluation requires 8–32 held-out cases')
    seen=set()
    for case in cases:
        msg=messages(case['messages']);require(msg[-1]['role']=='user','evaluation prompt must end with user')
        key=digest(msg);require(key not in bundle['training_prompts'] and key not in seen,'evaluation prompts must be held out and distinct');seen.add(key)
        require(isinstance(case.get('required',[]),list) and isinstance(case.get('forbidden',[]),list) and all(isinstance(x,str) and 0<len(x)<=1024 for x in case.get('required',[])+case.get('forbidden',[])),'invalid contract checks')
        require(len(case.get('required',[]))+len(case.get('forbidden',[]))<=32, 'too many response checks')
        require(type(case.get('python_ast',False))is bool,'invalid syntax check')
        require(case.get('expected_tool') is None or isinstance(case['expected_tool'],str) and 0<len(case['expected_tool'])<=128,'invalid expected tool')
        require(isinstance(case.get('required_arguments',[]),list) and len(case.get('required_arguments',[]))<=32 and all(isinstance(x,str) and 0<len(x)<=128 for x in case.get('required_arguments',[])), 'invalid tool argument contract')
        require(isinstance(case.get('expected_arguments',{}),dict) and len(case.get('expected_arguments',{}))<=32,'invalid exact argument contract')
        require(case.get('tools') is None or isinstance(case['tools'],list) and len(case['tools'])<=16 and all(isinstance(x,dict) for x in case['tools']), 'invalid evaluation tools')
        require(bool(case.get('required')) or bool(case.get('expected_tool')),'each held-out case needs a positive response contract')
    limit=request.get('max_tokens',128);require(type(limit)is int and 1<=limit<=256,'evaluation token limit exceeds budget')
    fingerprint=digest([bundle['id'],cases,request['strength'],limit,bundle['model'],bundle['adapter']]);state=resume(request,fingerprint,{'phase':'evaluating','completed':0,'total':len(cases),'results':[]})
    progress(request,state)
    model,tokenizer=load(request);require(len(layer_list(model))==bundle['layer_count'],'layer count changed')
    mx.eval(model(mx.array([[0]],dtype=mx.int32))) # warm weights before either condition is measured
    for index,case in enumerate(cases[state['completed']:],state['completed']):
        results={}
        for condition in (('baseline','steered') if index%2==0 else ('steered','baseline')):
            scope=hook(model,bundle['layer'],steering_callback(bundle['vector'],request['strength'])) if condition=='steered' else contextlib.nullcontext()
            with scope:results[condition]=generate_case(model,tokenizer,case,limit)
            mx.clear_cache()
        state['results'].append(results);state['completed']+=1;progress(request,state)
    baseline=sum(r['baseline']['passed'] for r in state['results']);steered=sum(r['steered']['passed'] for r in state['results'])
    tool_pass=all(not r['baseline']['tool_valid'] or r['steered']['tool_valid'] for r in state['results'])
    repeat_pass=sum(r['steered']['repeat_ratio'] for r in state['results'])<=sum(r['baseline']['repeat_ratio'] for r in state['results'])+0.01
    latency_base=sum(r['baseline']['first_token_secs'] for r in state['results']);latency_new=sum(r['steered']['first_token_secs'] for r in state['results'])
    passed=steered>baseline and steered/len(cases)>=0.8 and tool_pass and repeat_pass and latency_new<=latency_base*1.5+0.1
    evaluation={'passed':passed,'artifact':bundle['id'],'strength':request['strength'],'cases':len(cases),'baseline_passes':baseline,'steered_passes':steered,'tool_nonregression':tool_pass,'repetition_nonregression':repeat_pass,'latency_nonregression':latency_new<=latency_base*1.5+0.1,'baseline_first_token_secs':latency_base/len(cases),'steered_first_token_secs':latency_new/len(cases),'dataset':digest(cases),'scope':'held-out response contracts'}
    bundle['evaluation']=evaluation;atomic_json(request['output'],bundle);progress(request,dict(fingerprint=fingerprint,phase='evaluated',completed=len(cases),total=len(cases)))
    return evaluation


def operation(request):
    os.environ['HF_HUB_OFFLINE']='1'
    request=dict(request,model_name=request['model'])
    if not Path(request['model']).is_dir():
        from mlx_lm.utils import _download
        request['model']=str(_download(request['model']).resolve(strict=True))
    if request['op']=='steering_extract':return extract(request)
    if request['op']=='steering_evaluate':return evaluate(request)
    raise ValueError('unsupported steering operation')
