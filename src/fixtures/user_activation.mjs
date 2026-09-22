// WHATWG HTML e5071a2 #user-activation-processing-model / UserActivation.
(function () {
    const check=(ok,name)=>{if(!ok)throw Error(name);};
    const activation=navigator.userActivation;
    check(activation===navigator.userActivation && activation instanceof UserActivation,'SameObject identity');
    check(!activation.isActive && !activation.hasBeenActive,'initial activation');
    check(Object.prototype.toString.call(activation)==='[object UserActivation]','interface tag');
    for(const fn of [()=>new UserActivation(),()=>Object.getOwnPropertyDescriptor(UserActivation.prototype,'isActive').get.call({})]) {
        let error;try{fn();}catch(e){error=e;}check(error instanceof TypeError,'illegal constructor/receiver');
    }
    const html=document.createElement('html'),body=document.createElement('body');document.append(html);html.append(body);
    const first=document.createElement('iframe'),sibling=document.createElement('iframe');body.append(first,sibling);
    __trust.hydrateFrames();
    const child=first.contentWindow, other=sibling.contentWindow, button=first.contentDocument.createElement('button');
    const opaque=document.createElement('iframe'); opaque.src='data:text/html,<p>opaque</p>'; body.append(opaque); __trust.hydrateFrames();
    // Privileged harness reference; contentWindow remains subject to SOP.
    const opaqueWindow=opaque.__contentRealmWindow;
    first.contentDocument.body.append(button);
    const childActivation=child.navigator.userActivation;
    check(childActivation!==activation && childActivation instanceof child.UserActivation,'per-window identity');
    const getter=Object.getOwnPropertyDescriptor(UserActivation.prototype,'isActive').get;
    check(!getter.call(childActivation),'cross-realm brand');
    button.click(); button.dispatchEvent(new child.PointerEvent('pointerdown',{pointerType:'mouse',bubbles:true}));
    check(!activation.hasBeenActive && !childActivation.isActive,'untrusted input cannot activate');
    child.__trust.key(button.__id,'Escape','Escape',false,false,false,false,false,false);
    check(!activation.hasBeenActive,'Escape does not activate');
    let beforeDispatch=false;
    button.onpointerdown=()=>{beforeDispatch=childActivation.isActive && activation.isActive;};
    __trust.pointerButton(button.__id,true,5,6);
    __trust.pointerButton(button.__id,false,5,6);
    check(beforeDispatch && activation.hasBeenActive && childActivation.hasBeenActive,'activation before dispatch and ancestors');
    check(!other.navigator.userActivation.hasBeenActive,'no sibling activation');
    __trust.key(null,'a','KeyA',false,false,false,true,false,false);
    // Focus is still inside the first frame: a modified delivered key activates
    // its document and ancestors, not the ancestor's same-origin siblings.
    check(!other.navigator.userActivation.isActive,'modified key originating document');
    __trust.key(body.__id,'a','KeyA',false,false,false,true,false,false);
    check(other.navigator.userActivation.isActive,'same-origin descendants');
    check(!opaqueWindow.navigator.userActivation.hasBeenActive,'cross-origin descendant is excluded');
    opaqueWindow.__trust.key(null,'a','KeyA',false,false,false,false,false,false);
    check(opaqueWindow.navigator.userActivation.isActive && activation.isActive,'cross-origin input activates all ancestors');
    const saved=childActivation;
    first.srcdoc='<button>replacement</button>'; __trust.hydrateFrames();
    check(first.contentWindow.navigator.userActivation===saved,'initial about:blank reuses Window');
    first.srcdoc='<button>second document</button>'; __trust.hydrateFrames();
    check(first.contentWindow.navigator.userActivation!==saved && !first.contentWindow.navigator.userActivation.hasBeenActive,'new Window resets activation');
    check(saved.hasBeenActive,'retained old Window activation');
    return 'user-activation-ok';
})()
