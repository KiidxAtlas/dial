"""Steering admission/identity contracts; optional tiny real MLX hook tests."""
import copy
import json
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch
import dial_steering as s


def bundle():
    result=dict(version=1,model_name='test',model='a'*64,adapter=None,layer=0,layer_count=2,dimension=2,vector=[1.,0.],training_prompts=[s.digest([n]) for n in range(8)],pairs=8)
    result['id']=s.digest(s.payload(result))
    result['evaluation']=dict(passed=True,artifact=result['id'],strength=.5,cases=8,baseline_passes=4,steered_passes=7,scope='held-out response contracts',tool_nonregression=True,repetition_nonregression=True,latency_nonregression=True)
    return result


class Contracts(unittest.TestCase):
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

if __name__=='__main__':unittest.main()
