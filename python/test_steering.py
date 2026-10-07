"""Steering admission/identity contracts; optional tiny real MLX hook tests."""
import copy
import json
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch
from types import SimpleNamespace, ModuleType
import dial_steering as s


def bundle():
    result=dict(version=1,model_name='test',model='a'*64,adapter=None,layer=0,layer_count=2,dimension=2,vector=[1.,0.],training_prompts=[s.digest([n]) for n in range(8)],pairs=8)
    result['id']=s.digest(s.payload(result))
    result['evaluation']=dict(passed=True,artifact=result['id'],strength=.5,cases=8,baseline_passes=4,steered_passes=7,scope='held-out response contracts',tool_nonregression=True,repetition_nonregression=True,latency_nonregression=True)
    return result


class Contracts(unittest.TestCase):
    def test_multimodal_layers_always_target_the_language_decoder(self):
        language=[object(),object()];vision=[object()]
        model=SimpleNamespace(layers=vision,language_model=SimpleNamespace(model=SimpleNamespace(layers=language)))
        self.assertIs(s.layer_list(model),language)
        with self.assertRaisesRegex(ValueError,'mutable transformer layers'):
            s.layer_list(SimpleNamespace(layers=vision,language_model=object()))

    def vision_server(self, loader):
        generation=ModuleType('mlx_vlm.server.generation');generation.load_model_resources=loader
        routes={}
        app=SimpleNamespace(add_api_route=lambda path,handler,**kwargs:routes.update({path:handler}))
        return SimpleNamespace(app=app),generation,routes

    def test_vision_without_vector_does_not_change_loading_and_exposes_status(self):
        loader=lambda path,adapter:('model','processor','config')
        server,generation,routes=self.vision_server(loader)
        with patch.dict('sys.modules',{'mlx_vlm.server.generation':generation}):
            s.install_vision_server(server,None)
        self.assertIs(generation.load_model_resources,loader)
        self.assertEqual(routes['/dial/steering'](),dict(state='disabled',uses=0,engine='mlx-vlm'))

    def test_vision_rejected_vector_keeps_model_and_vision_layers_unmodified(self):
        model=SimpleNamespace(language_model=SimpleNamespace(layers=[object(),object()]),vision_tower=object())
        originals=list(model.language_model.layers);vision=model.vision_tower
        loader=lambda path,adapter:(model,'processor','config')
        for change in ('binding','admission','layer count'):
            with self.subTest(change=change), tempfile.TemporaryDirectory() as tmp:
                item=bundle()
                if change=='admission':item['evaluation']['passed']=False
                path=Path(tmp)/'vector.json';s.atomic_json(path,item)
                server,generation,routes=self.vision_server(loader)
                with patch.dict('sys.modules',{'mlx_vlm.server.generation':generation}), \
                     patch.object(s,'bind',side_effect=ValueError('model changed') if change=='binding' else None):
                    s.install_vision_server(server,{'path':str(path),'strength':.5,'id':item['id']})
                    if change=='layer count':model.language_model.layers=[object()]
                    self.assertEqual(generation.load_model_resources('test',None),(model,'processor','config'))
                status=routes['/dial/steering']()
                self.assertEqual(status['state'],'bypassed');self.assertEqual(status['uses'],0)
                self.assertIs(model.vision_tower,vision)
                if change!='layer count':self.assertEqual(model.language_model.layers,originals)
                model.language_model.layers=originals

    def test_admission_requires_exact_strength_improvement_and_all_regression_gates(self):
        original=bundle();s.validate(original,.5,True)
        for field,value in [('strength',1.),('steered_passes',4),('steered_passes',9),('tool_nonregression',False),('repetition_nonregression',False),('latency_nonregression',False),('artifact','b'*64)]:
            with self.subTest(field=field):
                item=copy.deepcopy(original);item['evaluation'][field]=value
                with self.assertRaises(ValueError):s.validate(item,.5,True)

    def test_vector_tampering_and_nonfinite_values_rejected(self):
        for vector in ([0.,1.],[float('nan'),0.],[0.,0.]):
            item=bundle();item['vector']=vector
            with self.assertRaises(ValueError):s.validate(item,.5,True)

    def test_model_metadata_change_invalidates_binding_large_tokenizer_is_bounded(self):
        with tempfile.TemporaryDirectory() as tmp:
            root=Path(tmp);(root/'config.json').write_text('{"hidden_size":2}')
            (root/'tokenizer.json').write_bytes(b' '* (s.MAX_BYTES+1))
            before=s.identity(root);(root/'config.json').write_text('{"hidden_size":3}')
            self.assertNotEqual(before,s.identity(root))

    def test_corrupt_or_finished_checkpoint_restarts_and_valid_checkpoint_resumes(self):
        with tempfile.TemporaryDirectory() as tmp:
            request={'progress':str(Path(tmp)/'progress.json')};initial={'phase':'extracting','completed':0,'total':8,'sum':None}
            s.atomic_json(request['progress'],dict(initial,fingerprint='test',completed=3,sum=[1.,2.]))
            self.assertEqual(s.resume(request,'test',initial)['completed'],3)
            for previous in (dict(initial,fingerprint='test',completed=9,sum=[1.]),dict(fingerprint='test',phase='awaiting evaluation',completed=8,total=8)):
                s.atomic_json(request['progress'],previous)
                self.assertEqual(s.resume(request,'test',initial)['completed'],0)

    def test_tool_contract_requires_complete_json_and_named_arguments(self):
        tok=type('Tok',(),{'tool_call_start':'<tool>','tool_call_end':'</tool>'})()
        case={'expected_tool':'read','required_arguments':['path']}
        self.assertTrue(s.check_case(case,'<tool>{"name":"read","arguments":{"path":"a"}}</tool>',tok)[0])
        self.assertFalse(s.check_case(case,'<tool>{"name":"read","arguments":{}}</tool>',tok)[0])
        self.assertFalse(s.check_case(case,'<tool>{"name":"read","arguments":{"path":"a"}}',tok)[0])

    def test_exact_tool_arguments_reject_wrong_values(self):
        tok=type('Tok',(),{'tool_call_start':None,'tool_call_end':None})()
        case={'expected_tool':'read','required_arguments':['path'],'expected_arguments':{'path':'src/right.rs'}}
        self.assertFalse(s.check_case(case,json.dumps({'name':'read','arguments':{'path':'src/wrong.rs'}}),tok)[0])
        self.assertTrue(s.check_case(case,json.dumps({'name':'read','arguments':{'path':'src/right.rs'}}),tok)[0])

    def test_contrast_accepts_verified_tool_messages_and_rejects_malformed_calls(self):
        good=[{'role':'user','content':'inspect a'}, {'role':'assistant','tool_calls':[{'id':'a','type':'function','function':{'name':'read','arguments':{'path':'a'}}}]}]
        self.assertEqual(s.messages(good,True),good)
        bad=copy.deepcopy(good);bad[-1]['tool_calls'][0]['function']['arguments']='broken'
        with self.assertRaises(ValueError):s.messages(bad,True)

    def test_backend_without_provider_starts_without_steering(self):
        from types import ModuleType
        server=ModuleType('mlx_vlm.server')
        def missing(name):
            raise AttributeError(f'no attribute {name}')
        server.__getattr__=missing
        s.install_server(server,None)
        self.assertNotIn('ModelProvider',vars(server))
        self.assertNotIn('APIHandler',vars(server))

    def test_backend_without_provider_rejects_explicit_steering(self):
        from types import ModuleType
        server=ModuleType('mlx_vlm.server')
        with self.assertRaisesRegex(s.SteeringError,'requires.*ModelProvider'):
            s.install_server(server,{'path':'unused'})

    def test_supported_backend_without_steering_preserves_load_and_status(self):
        import io
        class Provider:
            def _load(self,*args):pass
        class Handler:
            def do_GET(self):self.forwarded=True
            def send_response(self,status):self.status=status
            def send_header(self,*args):pass
            def end_headers(self):pass
        original=Provider._load
        server=type('Server',(),{'ModelProvider':Provider,'APIHandler':Handler})()
        s.install_server(server,None)
        self.assertIs(Provider._load,original)
        handler=Handler();handler.path='/dial/steering';handler.wfile=io.BytesIO()
        handler.do_GET()
        self.assertEqual(handler.status,200)
        self.assertEqual(json.loads(handler.wfile.getvalue()),s.STATUS)
        handler.path='/other';handler.do_GET();self.assertTrue(handler.forwarded)

    def test_server_mismatch_bypasses_hook_without_breaking_loaded_model(self):
        class Provider:
            def _load(self,*args):self.model=object();self.is_distributed=False;self.draft_model=None
        class Handler:
            def do_GET(self):pass
        server=type('Server',(),{'ModelProvider':Provider,'APIHandler':Handler})()
        with tempfile.TemporaryDirectory() as tmp:
            path=Path(tmp)/'vector.json';s.atomic_json(path,bundle())
            s.install_server(server,{'path':str(path),'strength':.5,'id':bundle()['id']})
            with patch.object(s,'bind',side_effect=ValueError('model changed')):
                provider=Provider();provider._load('test')
            self.assertIsNotNone(provider.model);self.assertEqual(s.STATUS['state'],'bypassed');self.assertEqual(s.STATUS['uses'],0)


class MetalHooks(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        try:
            import mlx.core as mx
            import mlx.nn as nn
            from mlx_lm.models.llama import Model,ModelArgs
        except ImportError as error:
            raise unittest.SkipTest(str(error))
        cls.mx=mx;cls.Model=Model;cls.ModelArgs=ModelArgs

    def model(self):
        return self.Model(self.ModelArgs(model_type='llama',hidden_size=16,num_hidden_layers=2,intermediate_size=32,num_attention_heads=2,num_key_value_heads=2,rms_norm_eps=1e-5,vocab_size=32))

    def test_real_transformer_steers_then_restores_and_supports_incremental_cache(self):
        mx=self.mx;model=self.model();tokens=mx.array([[1,2]],dtype=mx.int32);base=model(tokens);mx.eval(base)
        original=model.layers[0]
        with s.hook(model,0,s.steering_callback([1.]+[0.]*15,.5)):
            shifted=model(tokens);mx.eval(shifted)
            self.assertGreater(float(mx.max(mx.abs(base-shifted))),.0001)
            from mlx_lm.models.cache import make_prompt_cache
            cache=make_prompt_cache(model);mx.eval(model(tokens,cache=cache));mx.eval(model(mx.array([[3]],dtype=mx.int32),cache=cache))
        self.assertIs(model.layers[0],original)
        restored=model(tokens);mx.eval(restored);self.assertLess(float(mx.max(mx.abs(base-restored))),1e-6)
        with self.assertRaises(RuntimeError):
            with s.hook(model,0,s.steering_callback([1.]+[0.]*15,.5)):raise RuntimeError('cancel')
        self.assertIs(model.layers[0],original)

    def test_real_generation_and_server_confirmation(self):
        from tokenizers import Tokenizer
        from tokenizers.models import WordLevel
        from tokenizers.pre_tokenizers import Whitespace
        from transformers import PreTrainedTokenizerFast
        from mlx_lm.tokenizer_utils import TokenizerWrapper
        raw=Tokenizer(WordLevel({'<unk>':0,'<eos>':1,'hello':2,'user':3,'assistant':4},unk_token='<unk>'));raw.pre_tokenizer=Whitespace()
        hf=PreTrainedTokenizerFast(tokenizer_object=raw,unk_token='<unk>',eos_token='<eos>')
        hf.chat_template="{% for m in messages %}{{ m['role'] }} {{ m['content'] }} {% endfor %}{% if add_generation_prompt %}assistant{% endif %}"
        tokenizer=TokenizerWrapper(hf);model=self.model()
        with tempfile.TemporaryDirectory() as tmp:
            root=Path(tmp);model_dir=root/'model';model_dir.mkdir();(model_dir/'config.json').write_text('{}')
            item=bundle();item.update(model=s.identity(model_dir),dimension=16,vector=[1.]+[0.]*15);item['id']=s.digest(s.payload(item));item['evaluation']['artifact']=item['id']
            path=root/'candidate.json';s.atomic_json(path,item)
            class Provider:
                def _load(self,*args):self.model=model;self.is_distributed=False;self.draft_model=None
            class Handler:
                def do_GET(self):pass
            server=type('Server',(),{'ModelProvider':Provider,'APIHandler':Handler})()
            s.install_server(server,{'path':str(path),'strength':.5,'id':item['id']});provider=Provider();provider._load(str(model_dir))
            self.assertEqual(s.STATUS['state'],'loaded');self.assertEqual(s.STATUS['uses'],0)
            result=s.generate_case(model,tokenizer,{'messages':[{'role':'user','content':'hello'}],'required':['hello']},8)
            self.assertGreater(result['tokens'],0);self.assertEqual(s.STATUS['state'],'used');self.assertGreater(s.STATUS['uses'],0)

    def test_real_heldout_evaluation_records_failure_without_admission(self):
        from tokenizers import Tokenizer
        from tokenizers.models import WordLevel
        from tokenizers.pre_tokenizers import Whitespace
        from transformers import PreTrainedTokenizerFast
        from mlx_lm.tokenizer_utils import TokenizerWrapper
        raw=Tokenizer(WordLevel({'<unk>':0,'<eos>':1,'hello':2,'user':3,'assistant':4},unk_token='<unk>'));raw.pre_tokenizer=Whitespace()
        hf=PreTrainedTokenizerFast(tokenizer_object=raw,unk_token='<unk>',eos_token='<eos>')
        hf.chat_template="{% for m in messages %}{{ m['role'] }} {{ m['content'] }} {% endfor %}{% if add_generation_prompt %}assistant{% endif %}"
        tokenizer=TokenizerWrapper(hf);model=self.model();original=model.layers[0]
        with tempfile.TemporaryDirectory() as tmp:
            root=Path(tmp);model_dir=root/'model';model_dir.mkdir();(model_dir/'config.json').write_text('{}')
            item=bundle();item.update(model=s.identity(model_dir),dimension=16,vector=[1.]+[0.]*15);item['id']=s.digest(s.payload(item));item.pop('evaluation');path=root/'candidate.json';s.atomic_json(path,item)
            cases=[{'messages':[{'role':'user','content':f'held-out coding task {n}'}],'required':['unreachable_contract_value']} for n in range(8)]
            request=dict(model=str(model_dir),adapter=None,output=str(path),progress=str(root/'progress.json'),strength=.5,cases=cases,max_tokens=8)
            with patch.object(s,'load',return_value=(model,tokenizer)):result=s.evaluate(request)
            self.assertFalse(result['passed']);self.assertEqual(result['cases'],8);self.assertEqual(result['baseline_passes'],0)
            self.assertIs(model.layers[0],original);self.assertEqual(s.read_json(request['progress'])['completed'],8)
            with self.assertRaises(ValueError):s.validate(s.read_json(path),.5,True)

    def test_extraction_resume_produces_same_artifact_and_rejects_heldout_leak(self):
        mx=self.mx;model=self.model()
        class Tokenizer:
            def apply_chat_template(self,messages,**kwargs):return [1,2,int(messages[-1]['content'])] if messages[-1]['role']=='assistant' else [1,2]
        pairs=[{'positive':[{'role':'user','content':f'task {n}'},{'role':'assistant','content':'3'}],'negative':[{'role':'user','content':f'task {n}'},{'role':'assistant','content':'4'}]} for n in range(8)]
        with tempfile.TemporaryDirectory() as tmp:
            root=Path(tmp);model_dir=root/'model';model_dir.mkdir();(model_dir/'config.json').write_text('{}')
            req=dict(model=str(model_dir),model_name='test',adapter=None,pairs=pairs,layer=None,output=str(root/'candidate.json'),progress=str(root/'progress.json'))
            with patch.object(s,'load',return_value=(model,Tokenizer())):
                result=s.extract(req);self.assertEqual(result['pairs'],8);self.assertEqual(result['layer'],1)
                artifact=s.read_json(req['output'])['id']
                s.extract(req);self.assertEqual(s.read_json(req['output'])['id'],artifact)
                cases=[dict(messages=p['positive'][:-1],required=['ok']) for p in pairs]
                with self.assertRaisesRegex(ValueError,'held out'):s.evaluate(dict(req,cases=cases,strength=.5))

class VisionMetalHooks(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        try:
            import mlx.core as mx
            from mlx_vlm.models.qwen3_5_moe import Model
            from mlx_vlm.models.qwen3_5_moe.config import ModelConfig, TextConfig, VisionConfig
            from transformers import Qwen3_5Tokenizer
        except ImportError as error:
            raise unittest.SkipTest(str(error))
        cls.mx=mx;cls.Model=Model;cls.ModelConfig=ModelConfig
        cls.TextConfig=TextConfig;cls.VisionConfig=VisionConfig;cls.Tokenizer=Qwen3_5Tokenizer

    def model(self):
        text=self.TextConfig(model_type='qwen3_5_moe_text',hidden_size=32,num_hidden_layers=4,
            num_attention_heads=2,num_key_value_heads=1,head_dim=16,linear_num_value_heads=2,
            linear_num_key_heads=2,linear_key_head_dim=32,linear_value_head_dim=32,
            linear_conv_kernel_dim=4,num_experts=4,num_experts_per_tok=2,
            shared_expert_intermediate_size=32,moe_intermediate_size=32,rms_norm_eps=1e-6,
            vocab_size=64,max_position_embeddings=256,rope_parameters={'rope_type':'default',
            'rope_theta':10000,'mrope_section':[1,1,0],'partial_rotary_factor':.25})
        vision=self.VisionConfig(depth=1,hidden_size=32,intermediate_size=64,out_hidden_size=32,
            num_heads=2,patch_size=2,spatial_merge_size=2,temporal_patch_size=1,
            num_position_embeddings=16,deepstack_visual_indexes=[])
        return self.Model(self.ModelConfig(text_config=text,vision_config=vision,model_type='qwen3_5_moe',
            image_token_id=60,video_token_id=61,vision_start_token_id=62,vision_end_token_id=63,
            vocab_size=64,eos_token_id=1))

    def save_model(self,path):
        from dataclasses import asdict
        from mlx.utils import tree_flatten
        model=self.model();path.mkdir()
        (path/'config.json').write_text(json.dumps(asdict(model.config)))
        self.mx.save_safetensors(str(path/'model.safetensors'),dict(tree_flatten(model.parameters())))
        vocab={'<unk>':0,'<eos>':1}
        for char in 'abcdefghijklmnopqrstuvwxyz0123456789Ġ{}":[],':vocab[char]=len(vocab)
        while len(vocab)<60:vocab[f't{len(vocab)}']=len(vocab)
        markers=['<|image_pad|>','<|video_pad|>','<|vision_start|>','<|vision_end|>']
        vocab.update({marker:60+n for n,marker in enumerate(markers)})
        tokenizer=self.Tokenizer(vocab=vocab,merges=[],unk_token='<unk>',eos_token='<eos>',
            pad_token='<unk>',additional_special_tokens=markers)
        tokenizer.chat_template="{% for m in messages %}{{ m['role'] }} {{ m['content'] }} {% endfor %}{% if add_generation_prompt %}assistant{% endif %}"
        tokenizer.save_pretrained(path)
        (path/'processor_config.json').write_text('{"processor_class":"Qwen3VLProcessor"}')
        (path/'preprocessor_config.json').write_text('{"image_processor_type":"Qwen2VLImageProcessor","patch_size":2,"temporal_patch_size":1,"merge_size":2,"min_pixels":16,"max_pixels":64}')

    def test_image_logits_change_encoder_stays_identical_and_cache_restores(self):
        mx=self.mx;model=self.model();original=model.language_model.layers[1]
        tokens=mx.array([[62,60,63,3]],dtype=mx.int32);pixels=mx.ones((4,12));grid=mx.array([[1,2,2]])
        embeddings=model.get_input_embeddings(tokens,pixels,image_grid_thw=grid).inputs_embeds
        base=model(tokens,pixels,image_grid_thw=grid).logits;mx.eval(base,embeddings)
        with s.hook(model,1,s.steering_callback([1.]+[0.]*31,.5)):
            steered=model(tokens,pixels,image_grid_thw=grid).logits;mx.eval(steered)
            same=model.get_input_embeddings(tokens,pixels,image_grid_thw=grid).inputs_embeds
            self.assertLess(float(mx.max(mx.abs(embeddings-same))),1e-6)
            self.assertGreater(float(mx.max(mx.abs(base-steered))),.0001)
            cache=model.language_model.make_cache()
            mx.eval(model(tokens,pixels,cache=cache,image_grid_thw=grid).logits)
            mx.eval(model.language_model(mx.array([[3]],dtype=mx.int32),cache=cache).logits)
        self.assertIs(model.language_model.layers[1],original)
        restored=model(tokens,pixels,image_grid_thw=grid).logits
        self.assertLess(float(mx.max(mx.abs(base-restored))),1e-6)

    def test_saved_vision_worker_extracts_and_evaluates_without_admitting_failed_contracts(self):
        with tempfile.TemporaryDirectory() as tmp:
            root=Path(tmp);path=root/'model';self.save_model(path)
            pairs=[{'positive':[{'role':'user','content':f'task {n}'},{'role':'assistant','content':'hello'}],
                    'negative':[{'role':'user','content':f'task {n}'},{'role':'assistant','content':'other'}]} for n in range(8)]
            request=dict(model=str(path),model_name='tiny-vision',adapter=None,pairs=pairs,layer=1,
                output=str(root/'vector.json'),progress=str(root/'progress.json'))
            result=s.extract(request)
            self.assertEqual(result['dimension'],32);self.assertEqual(result['pairs'],8)
            self.assertEqual(result['layer'],1)
            cases=[dict(messages=[{'role':'user','content':f'held out {n}'}],required=['impossible_contract']) for n in range(8)]
            result=s.evaluate(dict(request,cases=cases,strength=.5,max_tokens=4))
            self.assertFalse(result['passed']);self.assertEqual(result['cases'],8)
            with self.assertRaises(ValueError):s.validate(s.read_json(request['output']),.5,True)

    def test_real_vision_server_thread_and_status_endpoint_confirm_use(self):
        import asyncio
        import httpx
        from fastapi import FastAPI
        from PIL import Image
        import mlx_vlm.server.generation as generation
        with tempfile.TemporaryDirectory() as tmp:
            root=Path(tmp);path=root/'model';self.save_model(path)
            item=bundle();item.update(model=s.identity(path),dimension=32,vector=[1.]+[0.]*31,layer=1,layer_count=4)
            item['id']=s.digest(s.payload(item));item['evaluation']['artifact']=item['id']
            vector=root/'vector.json';s.atomic_json(vector,item)
            server=SimpleNamespace(app=FastAPI())
            async def status():
                async with httpx.AsyncClient(transport=httpx.ASGITransport(app=server.app),base_url='http://test') as client:
                    response=await client.get('/dial/steering');self.assertEqual(response.status_code,200)
                    return response.json()
            with patch.object(generation,'load_model_resources',generation.load_model_resources):
                s.install_vision_server(server,{'path':str(vector),'strength':.5,'id':item['id']})
                runner=generation.ResponseGenerator(str(path))
                try:
                    runner.wait_until_ready(timeout=10)
                    self.assertEqual(asyncio.run(status())['state'],'loaded')
                    prompt='<|vision_start|><|image_pad|><|vision_end|> hello'
                    _,tokens=runner.generate(prompt,args=generation.GenerationArguments(max_tokens=4,temperature=0),images=[Image.new('RGB',(4,4))])
                    self.assertGreater(len(list(tokens)),0)
                    report=asyncio.run(status())
                    self.assertEqual(report['state'],'used');self.assertGreater(report['uses'],0)
                    self.assertEqual(report['engine'],'mlx-vlm')
                finally:runner.stop_and_join()


if __name__=='__main__':unittest.main()
