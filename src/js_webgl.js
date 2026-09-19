// Private bootstrap factory; consumed by the platform prelude before page code.
// WebGL 1.0 + Web IDL, official local snapshots 3b7a7538 and 8f18262.
globalThis.__trust_install_webgl = function(g, adapter) {
    const native = g.__webgl; delete g.__webgl;
    const apply = Reflect.apply, get = WeakMap.prototype.get, set = WeakMap.prototype.set;
    const slots = native(0,"slots",[],new WeakMap());
    const canvasContexts = native(0,"canvasSlots",[],new WeakMap());
    const create = Object.create, define = Object.defineProperty;
    const descriptor = Object.getOwnPropertyDescriptor, keys = Object.keys;
    const U8 = Uint8Array, U16 = Uint16Array, I32 = Int32Array, U32 = Uint32Array, F32 = Float32Array;
    const typedProto = Object.getPrototypeOf(U8.prototype);
    const typedBuffer = descriptor(typedProto,"buffer").get;
    const typedOffset = descriptor(typedProto,"byteOffset").get;
    const typedLength = descriptor(typedProto,"byteLength").get;
    const typedTag = descriptor(typedProto,Symbol.toStringTag).get;
    const copyBytes = U8.prototype.set;
    const objectFinalizer = new FinalizationRegistry(record => {
        const owner=record.owner.deref(),s=owner&&slot(owner);
        if(s&&s.epoch===record.epoch&&!s.lost){native(s.id,record.op,[record.id]);s.objects.delete(record.id);}
    });
    const contextFinalizer = new FinalizationRegistry(id => native(id,"dispose",[]));
    function slot(value) { return apply(get,slots,[value]); }
    function save(value,record) { apply(set,slots,[value,record]); return value; }
    function context(value) { const s=slot(value); if(!s||s.kind!=="Context")throw new TypeError("Illegal WebGLRenderingContext invocation");return s; }
    function lostValue(s,op) {
        if(op==="getError")return s.errors.length?s.errors.shift():0;
        if(op==="isContextLost")return true;
        if(op==="width"||op==="height"||op==="getVertexAttribOffset")return 0;
        if(op==="checkFramebufferStatus")return 0x8CDD;
        if(op==="getAttribLocation")return -1;
        if(op.startsWith("is"))return false;
        return null;
    }
    function call(s,op,n=[],payload) {
        if(s.lost)return lostValue(s,op);
        const result=native(s.id,op,n,payload);
        if((op==="isContextLost"&&result)||(op==="getError"&&result===0x9242)) {
            lose(s,false);if(op==="getError")s.errors=[];
        }
        return result;
    }
    class WebGLContextEvent extends adapter.Event {
        constructor(type,options={}) {required(arguments,1);super(type,options);save(this,{kind:"ContextEvent",message:options&&options.statusMessage!==undefined?`${options.statusMessage}`:""});}
        get statusMessage(){const r=slot(this);if(!r||r.kind!=="ContextEvent")throw new TypeError("Illegal WebGLContextEvent invocation");return r.message;}
    }
    g.WebGLContextEvent=WebGLContextEvent;
    function restore(s) {
        adapter.queue(()=>{
            if(!s.lost||!s.restorable)return;
            if(!native(s.id,"init",s.attributes,adapter.origin))return;
            s.lost=false;s.errors=[];s.restorable=false;s.simulated=false;
            adapter.fire(s.canvas,WebGLContextEvent,"webglcontextrestored","");
        });
    }
    function lose(s,simulated) {
        if(s.lost)return;
        s.lost=true;s.simulated=simulated;s.restorable=false;s.epoch++;s.errors=[0x9242];
        const ext=s.extensions.get("WEBGL_lose_context");s.extensions.clear();if(ext)s.extensions.set("WEBGL_lose_context",ext);
        s.refs.clear();s.objects.clear();native(s.id,"dispose",[]);
        adapter.queue(()=>{s.restorable=adapter.fire(s.canvas,WebGLContextEvent,"webglcontextlost","");if(s.restorable&&!simulated)restore(s);});
    }
    function error(s,code) { call(s,"error",[code]); }
    function required(args,n) { if(args.length<n)throw new TypeError("Not enough WebGL arguments"); }
    function number(value,type) { switch(type){case "u":return value>>>0;case "i":return value>>0;case "b":return +!!value;case "f":return Math.fround(+value);case "l":{const n=+value;return Number.isFinite(n)?Math.trunc(n):0;}default:return +value;} }
    function convert(args,signature) {required(args,signature.length);return Array.from(signature,(t,i)=>number(args[i],t));}
    function resource(s,value,kind,nullable=false) {
        if(value==null&&nullable)return 0;
        const r=slot(value);
        if(!r||r.kind!==kind)throw new TypeError("Expected WebGL"+kind);
        if(r.context!==s.owner||r.epoch!==s.epoch){if(!s.lost)error(s,1282);return -1;}
        return r.id;
    }
    class WebGLObject {constructor(){throw new TypeError("Illegal constructor");}get label(){const r=slot(this);if(!r||!r.object)throw new TypeError("Illegal WebGLObject invocation");return r.label;}set label(v){const r=slot(this);if(!r||!r.object)throw new TypeError("Illegal WebGLObject invocation");r.label=`${v}`;}}
    const constructors = {};
    for(const kind of ["Buffer","Framebuffer","Program","Renderbuffer","Shader","Texture"]) {
        const C=class extends WebGLObject{};define(C,"name",{value:"WebGL"+kind});
        define(C.prototype,Symbol.toStringTag,{value:"WebGL"+kind,configurable:true});constructors[kind]=C;g["WebGL"+kind]=C;
    }
    class WebGLUniformLocation {constructor(){throw new TypeError("Illegal constructor");}}
    constructors.UniformLocation=WebGLUniformLocation;g.WebGLUniformLocation=WebGLUniformLocation;g.WebGLObject=WebGLObject;
    function wrap(s,kind,id) {
        if(id==null||id===0)return null;
        const old=s.objects.get(id);const live=old&&old.deref();if(live)return live;
        const object=save(create(constructors[kind].prototype),{kind,id,context:s.owner,epoch:s.epoch,object:kind!=="UniformLocation",label:"",refs:new Map()});
        s.objects.set(id,new WeakRef(object));
        objectFinalizer.register(object,{owner:new WeakRef(s.owner),epoch:s.epoch,id,op:"delete"+kind},object);
        return object;
    }
    class WebGLActiveInfo {constructor(){throw new TypeError("Illegal constructor");}get size(){return info(this,"ActiveInfo")[0];}get type(){return info(this,"ActiveInfo")[1];}get name(){return info(this,"ActiveInfo")[2];}}
    class WebGLShaderPrecisionFormat {constructor(){throw new TypeError("Illegal constructor");}get rangeMin(){return info(this,"ShaderPrecisionFormat")[0];}get rangeMax(){return info(this,"ShaderPrecisionFormat")[1];}get precision(){return info(this,"ShaderPrecisionFormat")[2];}}
    function info(o,kind){const s=slot(o);if(!s||s.kind!==kind)throw new TypeError("Illegal WebGL info invocation");return s.data;}
    for(const C of [WebGLActiveInfo,WebGLShaderPrecisionFormat]){g[C.name]=C;define(C.prototype,Symbol.toStringTag,{value:C.name,configurable:true});}
    function makeInfo(C,kind,data){return data==null?null:save(create(C.prototype),{kind,data});}
    class WebGLRenderingContext {
        constructor(){throw new TypeError("Illegal constructor");}
        get canvas(){return context(this).canvas;}
        get drawingBufferWidth(){return call(context(this),"width");}
        get drawingBufferHeight(){return call(context(this),"height");}
        getContextAttributes(){const s=context(this);if(call(s,"isContextLost"))return null;const a=call(s,"attributes");return {alpha:a[0],depth:a[1],stencil:a[2],antialias:a[3],premultipliedAlpha:a[4],preserveDrawingBuffer:a[5],powerPreference:s.powerPreference,failIfMajorPerformanceCaveat:s.failCaveat,desynchronized:false};}
        getSupportedExtensions(){return call(context(this),"getSupportedExtensions");}
        getExtension(name){const s=context(this);required(arguments,1);name=`${name}`;if(name.toLowerCase()==="webgl_lose_context"&&s.extensions.has("WEBGL_lose_context"))return s.extensions.get("WEBGL_lose_context");const canonical=(this.getSupportedExtensions()||[]).find(n=>n.toLowerCase()===name.toLowerCase());if(!canonical)return null;if(s.extensions.has(canonical))return s.extensions.get(canonical);if(!call(s,"extension",[],canonical))return null;const ext={};
        if(canonical==="WEBGL_lose_context") {
            ext.loseContext=function(){if(this!==ext)throw new TypeError("Illegal extension invocation");if(s.lost){if(!s.errors.includes(1282))s.errors.push(1282);}else lose(s,true);};
            ext.restoreContext=function(){if(this!==ext)throw new TypeError("Illegal extension invocation");if(!s.lost||!s.restorable||!s.simulated){if(s.lost){if(!s.errors.includes(1282))s.errors.push(1282);}else error(s,1282);}else restore(s);};
        }
        if(canonical==="OES_standard_derivatives")define(ext,"FRAGMENT_SHADER_DERIVATIVE_HINT_OES",{value:0x8B8B,enumerable:true});if(canonical==="WEBGL_debug_renderer_info"){define(ext,"UNMASKED_VENDOR_WEBGL",{value:0x9245,enumerable:true});define(ext,"UNMASKED_RENDERER_WEBGL",{value:0x9246,enumerable:true});}if(canonical==="ANGLE_instanced_arrays") {
            define(ext,"VERTEX_ATTRIB_ARRAY_DIVISOR_ANGLE",{value:0x88FE,enumerable:true});
            for(const [name,sig] of Object.entries({drawArraysInstancedANGLE:"uiii",drawElementsInstancedANGLE:"uiuli",vertexAttribDivisorANGLE:"uu"})) {
                const epoch=s.epoch;const method=function(){if(this!==ext)throw new TypeError("Illegal extension invocation");const n=convert(arguments,sig);if(epoch!==s.epoch){if(!s.lost)error(s,1282);return;}call(s,name,n);};
                define(method,"name",{value:name});define(method,"length",{value:sig.length});ext[name]=method;
            }
        }s.extensions.set(canonical,ext);return ext;}
        getParameter(pname){const s=context(this);required(arguments,1);const p=pname>>>0;if((p===0x9245||p===0x9246)&&!s.extensions.has("WEBGL_debug_renderer_info")){error(s,1280);return null;}const result=call(s,"getParameter",[p]);if(result==null)return null;const kind=({34964:"Buffer",34965:"Buffer",35725:"Program",36006:"Framebuffer",36007:"Renderbuffer",32873:"Texture",34068:"Texture"})[p];if(kind)return wrap(s,kind,result);if([3386,2978,3088].includes(p))return new I32(result);if(p===34467)return new U32(result);if([33901,33902,32773,3106,2928].includes(p))return new F32(result);return result;}
        getShaderPrecisionFormat(type,precision){const s=context(this);const n=convert(arguments,"uu");return makeInfo(WebGLShaderPrecisionFormat,"ShaderPrecisionFormat",call(s,"getShaderPrecisionFormat",n));}
        bufferData(target,data,usage){const s=context(this);required(arguments,3);target=target>>>0;const numeric=typeof data!=="object"&&typeof data!=="function";const size=numeric?number(data,"l"):0;usage=usage>>>0;if(data===null){error(s,1281);return;}call(s,"bufferData",[target,size,usage],numeric?undefined:data);}
        bufferSubData(target,offset,data){const s=context(this);required(arguments,3);call(s,"bufferSubData",[target>>>0,number(offset,"l")],data);}
        shaderSource(shader,source){const s=context(this);required(arguments,2);const id=resource(s,shader,"Shader");source=`${source}`;if(id>=0)call(s,"shaderSource",[id],source);}
        getAttachedShaders(program){const s=context(this);required(arguments,1);const id=resource(s,program,"Program");if(id<0)return null;const ids=call(s,"getAttachedShaders",[id]);return ids&&ids.map(id=>wrap(s,"Shader",id));}
        getAttribLocation(program,name){const s=context(this);required(arguments,2);const id=resource(s,program,"Program");name=`${name}`;if(id<0)return -1;const value=call(s,"getAttribLocation",[id],name);return value==null?-1:value;}
        getUniformLocation(program,name){const s=context(this);required(arguments,2);const id=resource(s,program,"Program");name=`${name}`;return id<0?null:wrap(s,"UniformLocation",call(s,"getUniformLocation",[id],name));}
        bindAttribLocation(program,index,name){const s=context(this);required(arguments,3);const id=resource(s,program,"Program");index=index>>>0;name=`${name}`;if(id>=0)call(s,"bindAttribLocation",[id,index],name);}
        getUniform(program,location){const s=context(this);required(arguments,2);const p=resource(s,program,"Program"),l=resource(s,location,"UniformLocation");if(p<0||l<0)return null;const values=call(s,"getUniform",[p,l]);if(values==null)return null;const kind=call(s,"uniformType",[l]);if(kind===35670)return !!values[0];if([35671,35672,35673].includes(kind))return values.map(Boolean);if(values.length===1)return values[0];return [5124,35667,35668,35669,35678,35680].includes(kind)?new I32(values):new F32(values);}
        getVertexAttrib(index,pname){const s=context(this);const n=convert(arguments,"uu"),v=call(s,"getVertexAttrib",n);if(n[1]===34975)return wrap(s,"Buffer",v);if(n[1]===34342&&v!=null)return new F32(v);return v;}
        vertexAttribPointer(index,size,type,normalized,stride,offset){const s=context(this);const n=convert(arguments,"uiubil");call(s,"vertexAttribPointer",n);const id=call(s,"getVertexAttrib",[n[0],34975]);if(id!==null)s.refs.set("attrib"+n[0],wrap(s,"Buffer",id));}
        readPixels(x,y,width,height,format,type,pixels){const s=context(this);required(arguments,7);const n=convert(arguments,"iiiiuu");if(pixels==null){error(s,1281);return;}const tag=apply(typedTag,pixels,[]);if(n[5]!==5121||!(tag==="Uint8Array"||tag==="Uint8ClampedArray")){error(s,1282);return;}const buffer=apply(typedBuffer,pixels,[]),offset=apply(typedOffset,pixels,[]),length=apply(typedLength,pixels,[]);const dest=new U8(buffer,offset,length);const result=call(s,"readPixels",n,pixels);if(result)apply(copyBytes,dest,[result]);}
        texImage2D(){textureUpload(context(this),false,arguments);}
        texSubImage2D(){textureUpload(context(this),true,arguments);}
        framebufferRenderbuffer(target,attachment,renderbufferTarget,renderbuffer){const s=context(this);required(arguments,4);const n=[target>>>0,attachment>>>0,renderbufferTarget>>>0,resource(s,renderbuffer,"Renderbuffer",true)];if(n[3]<0)return;call(s,"framebufferRenderbuffer",n);retainAttachment(s,n[0],n[1]);}
        framebufferTexture2D(target,attachment,textarget,texture,level){const s=context(this);required(arguments,5);const n=[target>>>0,attachment>>>0,textarget>>>0,resource(s,texture,"Texture",true),level>>0];if(n[3]<0)return;call(s,"framebufferTexture2D",n);retainAttachment(s,n[0],n[1]);}
        getFramebufferAttachmentParameter(target,attachment,pname){const s=context(this);const n=convert(arguments,"uuu");const value=call(s,"getFramebufferAttachmentParameter",n);if(n[2]===36049&&value){const kind=call(s,"getFramebufferAttachmentParameter",[n[0],n[1],36048]);return wrap(s,kind===5890?"Texture":"Renderbuffer",value);}return value===0&&n[2]===36049?null:value;}
    }
    function retainAttachment(s,target,attachment) {
        if(target!==36160||![36064,36096,36128,33306].includes(attachment))return;
        const fb=wrap(s,"Framebuffer",call(s,"getParameter",[36006]));if(!fb)return;
        const type=call(s,"getFramebufferAttachmentParameter",[target,attachment,36048]);
        const object=type?wrap(s,type===5890?"Texture":"Renderbuffer",call(s,"getFramebufferAttachmentParameter",[target,attachment,36049])):null;
        slot(fb).refs.set(attachment,object);
    }
    for(const C of [WebGLObject,WebGLUniformLocation,WebGLActiveInfo,WebGLShaderPrecisionFormat,WebGLContextEvent]) {
        for(const name of Object.getOwnPropertyNames(C.prototype))if(name!=="constructor")define(C.prototype,name,{...descriptor(C.prototype,name),enumerable:true});
        define(C.prototype,Symbol.toStringTag,{value:C.name,configurable:true});
    }
    function method(name,fn,length){define(fn,"name",{value:name,configurable:true});define(fn,"length",{value:length,configurable:true});define(WebGLRenderingContext.prototype,name,{value:fn,writable:true,enumerable:true,configurable:true});}
    const simple={activeTexture:"u",blendColor:"ffff",blendEquation:"u",blendEquationSeparate:"uu",blendFunc:"uu",blendFuncSeparate:"uuuu",clear:"u",clearColor:"ffff",clearDepth:"f",clearStencil:"i",colorMask:"bbbb",cullFace:"u",depthFunc:"u",depthMask:"b",depthRange:"ff",disable:"u",enable:"u",frontFace:"u",hint:"uu",isEnabled:"u",lineWidth:"f",pixelStorei:"ui",polygonOffset:"ff",sampleCoverage:"fb",scissor:"iiii",stencilFunc:"uiu",stencilFuncSeparate:"uuiu",stencilMask:"u",stencilMaskSeparate:"uu",stencilOp:"uuu",stencilOpSeparate:"uuuu",viewport:"iiii",getError:"",isContextLost:"",finish:"",flush:"",getBufferParameter:"uu",getTexParameter:"uu",getRenderbufferParameter:"uu",getVertexAttribOffset:"uu",checkFramebufferStatus:"u",renderbufferStorage:"uuii",generateMipmap:"u",texParameteri:"uui",texParameterf:"uuf",drawArrays:"uii",drawElements:"uiul",copyTexImage2D:"uiuiiiii",copyTexSubImage2D:"uiiiiiii"};
    const queries=new Set(["getError","isContextLost","isEnabled","getBufferParameter","getTexParameter","getRenderbufferParameter","getVertexAttribOffset","checkFramebufferStatus"]);
    for(const name of keys(simple)){const sig=simple[name];method(name,function(){const s=context(this);const result=call(s,name,convert(arguments,sig));if(queries.has(name))return result;},sig.length);}
    for(const kind of ["Buffer","Framebuffer","Program","Renderbuffer","Shader","Texture"]){
        method("create"+kind,function(){const s=context(this);const n=kind==="Shader"?convert(arguments,"u"):[];return wrap(s,kind,call(s,"create"+kind,n));},kind==="Shader"?1:0);
        method("delete"+kind,function(value){const s=context(this);required(arguments,1);const id=resource(s,value,kind,true);if(id<0)return;call(s,"delete"+kind,[id]);if(value){objectFinalizer.unregister(value);if(kind!=="Program"||!call(s,"isProgram",[id]))for(const [key,ref] of s.refs)if(ref===value)s.refs.delete(key);}},1);
        method("is"+kind,function(value){const s=context(this);required(arguments,1);if(value==null)return false;const r=slot(value);if(!r||r.kind!==kind)throw new TypeError("Invalid WebGL object");if(r.context!==s.owner||r.epoch!==s.epoch)return false;return call(s,"is"+kind,[r.id]);},1);
    }
    for(const kind of ["Buffer","Framebuffer","Renderbuffer","Texture"]){method("bind"+kind,function(target,value){const s=context(this);required(arguments,2);target=target>>>0;const id=resource(s,value,kind,true);if(id<0)return;call(s,"bind"+kind,[target,id]);const p=({34962:34964,34963:34965,36160:36006,36161:36007,3553:32873,34067:34068})[target];if(p&&call(s,"getParameter",[p])===id)s.refs.set(kind+":"+target+(kind==="Texture"?":"+call(s,"getParameter",[34016]):""),value);},2);}
    for(const name of ["compileShader","getShaderInfoLog","getShaderSource","getShaderParameter","linkProgram","validateProgram","getProgramInfoLog","getProgramParameter","useProgram"]){const kind=name.includes("Shader")?"Shader":"Program";const count=name.endsWith("Parameter")?2:1;method(name,function(value,pname){const s=context(this);required(arguments,count);const id=resource(s,value,kind,name==="useProgram");const n=[id];if(count===2)n.push(pname>>>0);if(id<0)return null;const result=call(s,name,n);if(name==="useProgram"&&call(s,"getParameter",[35725])===id)s.refs.set("program",value);if(name.startsWith("get"))return result;},count);}
    for(const name of ["attachShader","detachShader"]){method(name,function(program,shader){const s=context(this);required(arguments,2);const p=resource(s,program,"Program"),id=resource(s,shader,"Shader");if(p<0||id<0)return;call(s,name,[p,id]);if(name==="attachShader")slot(program).refs.set(id,shader);else slot(program).refs.delete(id);},2);}
    for(const name of ["getActiveAttrib","getActiveUniform"]){method(name,function(program,index){const s=context(this);required(arguments,2);const p=resource(s,program,"Program");index=index>>>0;if(p<0)return null;return makeInfo(WebGLActiveInfo,"ActiveInfo",call(s,name,[p,index]));},2);}
    for(let width=1;width<=4;width++){
        for(const integer of [false,true])for(const vector of [false,true]){const name="uniform"+width+(integer?"i":"f")+(vector?"v":"");method(name,function(location){const s=context(this);required(arguments,vector?2:width+1);const id=resource(s,location,"UniformLocation",true);const values=vector?Array.from(arguments[1],v=>number(v,integer?"i":"f")):Array.from({length:width},(_,i)=>number(arguments[i+1],integer?"i":"f"));if(id>=0)call(s,"uniform",[id,width,+integer,0,...values]);},vector?2:width+1);}
        for(const vector of [false,true]){const name="vertexAttrib"+width+"f"+(vector?"v":"");method(name,function(index){const s=context(this);required(arguments,vector?2:width+1);index=index>>>0;const values=vector?Array.from(arguments[1],v=>number(v,"f")):Array.from({length:width},(_,i)=>number(arguments[i+1],"f"));if(values.length<width){error(s,1281);return;}const v=[0,0,0,1];for(let i=0;i<width;i++)v[i]=values[i];call(s,"vertexAttrib",[index,...v]);},vector?2:width+1);}
    }
    for(let width=2;width<=4;width++){method("uniformMatrix"+width+"fv",function(location,transpose,data){const s=context(this);required(arguments,3);const id=resource(s,location,"UniformLocation",true);transpose=!!transpose;const values=Array.from(data,v=>number(v,"f"));if(id===0)return;if(transpose){error(s,1281);return;}if(id>=0)call(s,"uniform",[id,width*width,0,1,...values]);},3);}
    for(const name of ["enableVertexAttribArray","disableVertexAttribArray"]){method(name,function(index){const s=context(this);call(s,name,convert(arguments,"u"));},1);}
    for(const name of ["compressedTexImage2D","compressedTexSubImage2D"]){method(name,function(){const s=context(this);required(arguments,name==="compressedTexImage2D"?7:8);error(s,1280);},name==="compressedTexImage2D"?7:8);}
    function textureUpload(s,sub,args){
        const dom=args.length===(sub?7:6);required(args,dom?(sub?7:6):9);
        let n,pixels;
        if(dom){const target=args[0]>>>0,level=args[1]>>0;const x=sub?args[2]>>0:0,y=sub?args[3]>>0:0;const format=args[sub?4:3]>>>0,type=args[sub?5:4]>>>0,internal=sub?format:args[2]>>0;
            const source=adapter.source(args[sub?6:5]);if(!source){error(s,1281);return;}if(!source.clean)throw new DOMException("Texture source is not origin-clean","SecurityError");
            pixels=source.data;n=sub?[target,level,x,y,source.width,source.height,format,type,source.bitmap?2:1]:[target,level,internal,source.width,source.height,0,format,type,source.bitmap?2:1];
        }else{n=convert(args,sub?"uiiiiiuu":"uiiiiiuu");pixels=args[8];n.push(0);if(pixels!=null){const tag=apply(typedTag,pixels,[]);if(n[7]===5121?!(tag==="Uint8Array"||tag==="Uint8ClampedArray"):tag!=="Uint16Array"){error(s,1282);return;}}}
        call(s,sub?"texSubImage2D":"texImage2D",n,pixels);
    }
    const constants = {
        DEPTH_BUFFER_BIT:0x00000100,
        STENCIL_BUFFER_BIT:0x00000400,
        COLOR_BUFFER_BIT:0x00004000,
        POINTS:0x0000,
        LINES:0x0001,
        LINE_LOOP:0x0002,
        LINE_STRIP:0x0003,
        TRIANGLES:0x0004,
        TRIANGLE_STRIP:0x0005,
        TRIANGLE_FAN:0x0006,
        ZERO:0,
        ONE:1,
        SRC_COLOR:0x0300,
        ONE_MINUS_SRC_COLOR:0x0301,
        SRC_ALPHA:0x0302,
        ONE_MINUS_SRC_ALPHA:0x0303,
        DST_ALPHA:0x0304,
        ONE_MINUS_DST_ALPHA:0x0305,
        DST_COLOR:0x0306,
        ONE_MINUS_DST_COLOR:0x0307,
        SRC_ALPHA_SATURATE:0x0308,
        FUNC_ADD:0x8006,
        BLEND_EQUATION:0x8009,
        BLEND_EQUATION_RGB:0x8009,
        BLEND_EQUATION_ALPHA:0x883D,
        FUNC_SUBTRACT:0x800A,
        FUNC_REVERSE_SUBTRACT:0x800B,
        BLEND_DST_RGB:0x80C8,
        BLEND_SRC_RGB:0x80C9,
        BLEND_DST_ALPHA:0x80CA,
        BLEND_SRC_ALPHA:0x80CB,
        CONSTANT_COLOR:0x8001,
        ONE_MINUS_CONSTANT_COLOR:0x8002,
        CONSTANT_ALPHA:0x8003,
        ONE_MINUS_CONSTANT_ALPHA:0x8004,
        BLEND_COLOR:0x8005,
        ARRAY_BUFFER:0x8892,
        ELEMENT_ARRAY_BUFFER:0x8893,
        ARRAY_BUFFER_BINDING:0x8894,
        ELEMENT_ARRAY_BUFFER_BINDING:0x8895,
        STREAM_DRAW:0x88E0,
        STATIC_DRAW:0x88E4,
        DYNAMIC_DRAW:0x88E8,
        BUFFER_SIZE:0x8764,
        BUFFER_USAGE:0x8765,
        CURRENT_VERTEX_ATTRIB:0x8626,
        FRONT:0x0404,
        BACK:0x0405,
        FRONT_AND_BACK:0x0408,
        CULL_FACE:0x0B44,
        BLEND:0x0BE2,
        DITHER:0x0BD0,
        STENCIL_TEST:0x0B90,
        DEPTH_TEST:0x0B71,
        SCISSOR_TEST:0x0C11,
        POLYGON_OFFSET_FILL:0x8037,
        SAMPLE_ALPHA_TO_COVERAGE:0x809E,
        SAMPLE_COVERAGE:0x80A0,
        NO_ERROR:0,
        INVALID_ENUM:0x0500,
        INVALID_VALUE:0x0501,
        INVALID_OPERATION:0x0502,
        OUT_OF_MEMORY:0x0505,
        CW:0x0900,
        CCW:0x0901,
        LINE_WIDTH:0x0B21,
        ALIASED_POINT_SIZE_RANGE:0x846D,
        ALIASED_LINE_WIDTH_RANGE:0x846E,
        CULL_FACE_MODE:0x0B45,
        FRONT_FACE:0x0B46,
        DEPTH_RANGE:0x0B70,
        DEPTH_WRITEMASK:0x0B72,
        DEPTH_CLEAR_VALUE:0x0B73,
        DEPTH_FUNC:0x0B74,
        STENCIL_CLEAR_VALUE:0x0B91,
        STENCIL_FUNC:0x0B92,
        STENCIL_FAIL:0x0B94,
        STENCIL_PASS_DEPTH_FAIL:0x0B95,
        STENCIL_PASS_DEPTH_PASS:0x0B96,
        STENCIL_REF:0x0B97,
        STENCIL_VALUE_MASK:0x0B93,
        STENCIL_WRITEMASK:0x0B98,
        STENCIL_BACK_FUNC:0x8800,
        STENCIL_BACK_FAIL:0x8801,
        STENCIL_BACK_PASS_DEPTH_FAIL:0x8802,
        STENCIL_BACK_PASS_DEPTH_PASS:0x8803,
        STENCIL_BACK_REF:0x8CA3,
        STENCIL_BACK_VALUE_MASK:0x8CA4,
        STENCIL_BACK_WRITEMASK:0x8CA5,
        VIEWPORT:0x0BA2,
        SCISSOR_BOX:0x0C10,
        COLOR_CLEAR_VALUE:0x0C22,
        COLOR_WRITEMASK:0x0C23,
        UNPACK_ALIGNMENT:0x0CF5,
        PACK_ALIGNMENT:0x0D05,
        MAX_TEXTURE_SIZE:0x0D33,
        MAX_VIEWPORT_DIMS:0x0D3A,
        SUBPIXEL_BITS:0x0D50,
        RED_BITS:0x0D52,
        GREEN_BITS:0x0D53,
        BLUE_BITS:0x0D54,
        ALPHA_BITS:0x0D55,
        DEPTH_BITS:0x0D56,
        STENCIL_BITS:0x0D57,
        POLYGON_OFFSET_UNITS:0x2A00,
        POLYGON_OFFSET_FACTOR:0x8038,
        TEXTURE_BINDING_2D:0x8069,
        SAMPLE_BUFFERS:0x80A8,
        SAMPLES:0x80A9,
        SAMPLE_COVERAGE_VALUE:0x80AA,
        SAMPLE_COVERAGE_INVERT:0x80AB,
        COMPRESSED_TEXTURE_FORMATS:0x86A3,
        DONT_CARE:0x1100,
        FASTEST:0x1101,
        NICEST:0x1102,
        GENERATE_MIPMAP_HINT:0x8192,
        BYTE:0x1400,
        UNSIGNED_BYTE:0x1401,
        SHORT:0x1402,
        UNSIGNED_SHORT:0x1403,
        INT:0x1404,
        UNSIGNED_INT:0x1405,
        FLOAT:0x1406,
        DEPTH_COMPONENT:0x1902,
        ALPHA:0x1906,
        RGB:0x1907,
        RGBA:0x1908,
        LUMINANCE:0x1909,
        LUMINANCE_ALPHA:0x190A,
        UNSIGNED_SHORT_4_4_4_4:0x8033,
        UNSIGNED_SHORT_5_5_5_1:0x8034,
        UNSIGNED_SHORT_5_6_5:0x8363,
        FRAGMENT_SHADER:0x8B30,
        VERTEX_SHADER:0x8B31,
        MAX_VERTEX_ATTRIBS:0x8869,
        MAX_VERTEX_UNIFORM_VECTORS:0x8DFB,
        MAX_VARYING_VECTORS:0x8DFC,
        MAX_COMBINED_TEXTURE_IMAGE_UNITS:0x8B4D,
        MAX_VERTEX_TEXTURE_IMAGE_UNITS:0x8B4C,
        MAX_TEXTURE_IMAGE_UNITS:0x8872,
        MAX_FRAGMENT_UNIFORM_VECTORS:0x8DFD,
        SHADER_TYPE:0x8B4F,
        DELETE_STATUS:0x8B80,
        LINK_STATUS:0x8B82,
        VALIDATE_STATUS:0x8B83,
        ATTACHED_SHADERS:0x8B85,
        ACTIVE_UNIFORMS:0x8B86,
        ACTIVE_ATTRIBUTES:0x8B89,
        SHADING_LANGUAGE_VERSION:0x8B8C,
        CURRENT_PROGRAM:0x8B8D,
        NEVER:0x0200,
        LESS:0x0201,
        EQUAL:0x0202,
        LEQUAL:0x0203,
        GREATER:0x0204,
        NOTEQUAL:0x0205,
        GEQUAL:0x0206,
        ALWAYS:0x0207,
        KEEP:0x1E00,
        REPLACE:0x1E01,
        INCR:0x1E02,
        DECR:0x1E03,
        INVERT:0x150A,
        INCR_WRAP:0x8507,
        DECR_WRAP:0x8508,
        VENDOR:0x1F00,
        RENDERER:0x1F01,
        VERSION:0x1F02,
        NEAREST:0x2600,
        LINEAR:0x2601,
        NEAREST_MIPMAP_NEAREST:0x2700,
        LINEAR_MIPMAP_NEAREST:0x2701,
        NEAREST_MIPMAP_LINEAR:0x2702,
        LINEAR_MIPMAP_LINEAR:0x2703,
        TEXTURE_MAG_FILTER:0x2800,
        TEXTURE_MIN_FILTER:0x2801,
        TEXTURE_WRAP_S:0x2802,
        TEXTURE_WRAP_T:0x2803,
        TEXTURE_2D:0x0DE1,
        TEXTURE:0x1702,
        TEXTURE_CUBE_MAP:0x8513,
        TEXTURE_BINDING_CUBE_MAP:0x8514,
        TEXTURE_CUBE_MAP_POSITIVE_X:0x8515,
        TEXTURE_CUBE_MAP_NEGATIVE_X:0x8516,
        TEXTURE_CUBE_MAP_POSITIVE_Y:0x8517,
        TEXTURE_CUBE_MAP_NEGATIVE_Y:0x8518,
        TEXTURE_CUBE_MAP_POSITIVE_Z:0x8519,
        TEXTURE_CUBE_MAP_NEGATIVE_Z:0x851A,
        MAX_CUBE_MAP_TEXTURE_SIZE:0x851C,
        TEXTURE0:0x84C0,
        TEXTURE1:0x84C1,
        TEXTURE2:0x84C2,
        TEXTURE3:0x84C3,
        TEXTURE4:0x84C4,
        TEXTURE5:0x84C5,
        TEXTURE6:0x84C6,
        TEXTURE7:0x84C7,
        TEXTURE8:0x84C8,
        TEXTURE9:0x84C9,
        TEXTURE10:0x84CA,
        TEXTURE11:0x84CB,
        TEXTURE12:0x84CC,
        TEXTURE13:0x84CD,
        TEXTURE14:0x84CE,
        TEXTURE15:0x84CF,
        TEXTURE16:0x84D0,
        TEXTURE17:0x84D1,
        TEXTURE18:0x84D2,
        TEXTURE19:0x84D3,
        TEXTURE20:0x84D4,
        TEXTURE21:0x84D5,
        TEXTURE22:0x84D6,
        TEXTURE23:0x84D7,
        TEXTURE24:0x84D8,
        TEXTURE25:0x84D9,
        TEXTURE26:0x84DA,
        TEXTURE27:0x84DB,
        TEXTURE28:0x84DC,
        TEXTURE29:0x84DD,
        TEXTURE30:0x84DE,
        TEXTURE31:0x84DF,
        ACTIVE_TEXTURE:0x84E0,
        REPEAT:0x2901,
        CLAMP_TO_EDGE:0x812F,
        MIRRORED_REPEAT:0x8370,
        FLOAT_VEC2:0x8B50,
        FLOAT_VEC3:0x8B51,
        FLOAT_VEC4:0x8B52,
        INT_VEC2:0x8B53,
        INT_VEC3:0x8B54,
        INT_VEC4:0x8B55,
        BOOL:0x8B56,
        BOOL_VEC2:0x8B57,
        BOOL_VEC3:0x8B58,
        BOOL_VEC4:0x8B59,
        FLOAT_MAT2:0x8B5A,
        FLOAT_MAT3:0x8B5B,
        FLOAT_MAT4:0x8B5C,
        SAMPLER_2D:0x8B5E,
        SAMPLER_CUBE:0x8B60,
        VERTEX_ATTRIB_ARRAY_ENABLED:0x8622,
        VERTEX_ATTRIB_ARRAY_SIZE:0x8623,
        VERTEX_ATTRIB_ARRAY_STRIDE:0x8624,
        VERTEX_ATTRIB_ARRAY_TYPE:0x8625,
        VERTEX_ATTRIB_ARRAY_NORMALIZED:0x886A,
        VERTEX_ATTRIB_ARRAY_POINTER:0x8645,
        VERTEX_ATTRIB_ARRAY_BUFFER_BINDING:0x889F,
        IMPLEMENTATION_COLOR_READ_TYPE:0x8B9A,
        IMPLEMENTATION_COLOR_READ_FORMAT:0x8B9B,
        COMPILE_STATUS:0x8B81,
        LOW_FLOAT:0x8DF0,
        MEDIUM_FLOAT:0x8DF1,
        HIGH_FLOAT:0x8DF2,
        LOW_INT:0x8DF3,
        MEDIUM_INT:0x8DF4,
        HIGH_INT:0x8DF5,
        FRAMEBUFFER:0x8D40,
        RENDERBUFFER:0x8D41,
        RGBA4:0x8056,
        RGB5_A1:0x8057,
        RGBA8:0x8058,
        RGB565:0x8D62,
        DEPTH_COMPONENT16:0x81A5,
        STENCIL_INDEX8:0x8D48,
        DEPTH_STENCIL:0x84F9,
        RENDERBUFFER_WIDTH:0x8D42,
        RENDERBUFFER_HEIGHT:0x8D43,
        RENDERBUFFER_INTERNAL_FORMAT:0x8D44,
        RENDERBUFFER_RED_SIZE:0x8D50,
        RENDERBUFFER_GREEN_SIZE:0x8D51,
        RENDERBUFFER_BLUE_SIZE:0x8D52,
        RENDERBUFFER_ALPHA_SIZE:0x8D53,
        RENDERBUFFER_DEPTH_SIZE:0x8D54,
        RENDERBUFFER_STENCIL_SIZE:0x8D55,
        FRAMEBUFFER_ATTACHMENT_OBJECT_TYPE:0x8CD0,
        FRAMEBUFFER_ATTACHMENT_OBJECT_NAME:0x8CD1,
        FRAMEBUFFER_ATTACHMENT_TEXTURE_LEVEL:0x8CD2,
        FRAMEBUFFER_ATTACHMENT_TEXTURE_CUBE_MAP_FACE:0x8CD3,
        COLOR_ATTACHMENT0:0x8CE0,
        DEPTH_ATTACHMENT:0x8D00,
        STENCIL_ATTACHMENT:0x8D20,
        DEPTH_STENCIL_ATTACHMENT:0x821A,
        NONE:0,
        FRAMEBUFFER_COMPLETE:0x8CD5,
        FRAMEBUFFER_INCOMPLETE_ATTACHMENT:0x8CD6,
        FRAMEBUFFER_INCOMPLETE_MISSING_ATTACHMENT:0x8CD7,
        FRAMEBUFFER_INCOMPLETE_DIMENSIONS:0x8CD9,
        FRAMEBUFFER_UNSUPPORTED:0x8CDD,
        FRAMEBUFFER_BINDING:0x8CA6,
        RENDERBUFFER_BINDING:0x8CA7,
        MAX_RENDERBUFFER_SIZE:0x84E8,
        INVALID_FRAMEBUFFER_OPERATION:0x0506,
        UNPACK_FLIP_Y_WEBGL:0x9240,
        UNPACK_PREMULTIPLY_ALPHA_WEBGL:0x9241,
        CONTEXT_LOST_WEBGL:0x9242,
        UNPACK_COLORSPACE_CONVERSION_WEBGL:0x9243,
        BROWSER_DEFAULT_WEBGL:0x9244,
    };
    for(const [name,value] of Object.entries(constants))for(const target of [WebGLRenderingContext,WebGLRenderingContext.prototype])define(target,name,{value,enumerable:true});
    for(const name of Object.getOwnPropertyNames(WebGLRenderingContext.prototype)){if(name!=="constructor")define(WebGLRenderingContext.prototype,name,{...descriptor(WebGLRenderingContext.prototype,name),enumerable:true});}
    define(WebGLRenderingContext.prototype,Symbol.toStringTag,{value:"WebGLRenderingContext",configurable:true});g.WebGLRenderingContext=WebGLRenderingContext;
    return {contexts:canvasContexts,is(value){const s=slot(value);return !!s&&s.kind==="Context";},create(canvas,options){
        if(options!=null&&typeof options!=="object"&&typeof options!=="function")throw new TypeError("Expected WebGL context settings");options=options||{};
        // Web IDL dictionary members are read in lexicographic order.
        const out={};for(const name of ["alpha","antialias","depth","desynchronized","failIfMajorPerformanceCaveat","powerPreference","premultipliedAlpha","preserveDrawingBuffer","stencil"]){const v=options[name];out[name]=v===undefined?({alpha:true,antialias:true,depth:true,powerPreference:"default",premultipliedAlpha:true})[name]:name==="powerPreference"?`${v}`:!!v;if(name==="powerPreference"&&!["default","low-power","high-performance"].includes(out[name]))throw new TypeError("Invalid powerPreference");}
        const id=adapter.identity(canvas),attributes=[+out.alpha,+out.depth,+!!out.stencil,+out.premultipliedAlpha,+!!out.preserveDrawingBuffer,+!!out.failIfMajorPerformanceCaveat];
        if(!native(id,"init",attributes,adapter.origin)){adapter.fire(canvas,WebGLContextEvent,"webglcontextcreationerror","A robust EGL/GLES drawing buffer could not be created");return null;}
        const owner=create(WebGLRenderingContext.prototype),s={kind:"Context",owner,canvas,id,attributes,epoch:0,lost:false,errors:[],objects:new Map(),refs:new Map(),extensions:new Map(),powerPreference:out.powerPreference,failCaveat:!!out.failIfMajorPerformanceCaveat};save(owner,s);contextFinalizer.register(owner,id);apply(set,canvasContexts,[canvas,owner]);return owner;
    }};
};
