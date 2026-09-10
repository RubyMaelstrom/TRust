(() => {
    const check = (value, message) => { if (!value) throw Error(message); };
    const throws = (fn, name) => {
        try { fn(); } catch (e) { check(e.name === name, name + ': ' + e); return; }
        throw Error('Missing ' + name);
    };
    const html = document.createElement('html'); document.appendChild(html);
    const body = document.createElement('body'); html.appendChild(body);
    body.innerHTML = `<style id="s">
        @property --Size { syntax: "<length>"; inherits: false; initial-value: 1in; }
        @property --inherit { syntax: "<length>"; inherits: true; initial-value: 3px; }
        @property --empty { initial-value: ; }
        @property --missing { syntax: "<number>"; }
        #p { font-size:20px; --Size:2em; --inherit:3em }
        #c { font-size:10px; width:var(--Size); height:var(--inherit) }
        #c::before { content:"x"; --Size:2em; font-size:4px }
        </style><div id="p"><div id="c"></div></div>`;
    const c = document.getElementById('c'), p = document.getElementById('p');
    const sheet = document.getElementById('s').sheet;
    const value = (name, element = c) => getComputedStyle(element).getPropertyValue(name);
    const rule = sheet.cssRules[0];
    check(rule instanceof CSSPropertyRule, 'rule interface');
    check(rule.name === '--Size' && rule.syntax === '<length>' && rule.inherits === false && rule.initialValue === '1in', 'rule descriptors');
    check(rule.cssText === '@property --Size { syntax: "<length>"; inherits: false; initial-value: 1in; }', 'rule serialization');
    check(sheet.cssRules[2].initialValue === '' && sheet.cssRules[3].initialValue === null, 'empty versus omitted descriptor');
    check(value('--Size') === '96px' && value('--Size',p) === '40px', 'typed computation and case');
    check(value('--inherit') === '60px' && getComputedStyle(c).width === '96px', 'inherit computed parent value');
    check(getComputedStyle(c,'::before').getPropertyValue('--Size') === '8px', 'pseudo computed units');
    check(getComputedStyle(c,':before').getPropertyValue('--inherit') === '60px', 'pseudo inherited value');
    const live = getComputedStyle(c);
    check(Array.from(live).includes('--empty') && !Array.from(live).includes('--missing'), 'computed custom property enumeration');
    c.style.setProperty('--Size', 'red');
    check(value('--Size') === '96px' && c.style.getPropertyValue('--Size') === 'red', 'defer syntax validation to computation');
    check(CSS.supports('--Size','red') && CSS.supports('(--Size:red)'), 'supports independent of registration');
    CSS.registerProperty({name:'--Size',syntax:'<number>',inherits:false,initialValue:'7'});
    check(value('--Size') === '7' && live.getPropertyValue('--Size') === '7', 'JS override and live invalidation');
    throws(() => CSS.registerProperty({name:'--Size',syntax:'bad'}),'InvalidModificationError');
    throws(() => CSS.registerProperty({name:'color'}),'SyntaxError');
    throws(() => CSS.registerProperty({name:'--bad',syntax:'<length>',initialValue:'1em'}),'SyntaxError');
    throws(() => CSS.registerProperty({name:'--bad',syntax:'<number>',initialValue:'var(--missing)'}),'SyntaxError');
    throws(() => CSS.registerProperty({name:'--bad',syntax:'<unknown>'}),'SyntaxError');
    CSS.registerProperty({name:'--bad'});
    throws(() => CSS.registerProperty(),'TypeError');
    throws(() => CSS.registerProperty(null),'TypeError');
    throws(() => CSS.registerProperty(7),'TypeError');
    throws(() => CSS.registerProperty({name:Symbol('x')}),'TypeError');
    const order = [];
    CSS.registerProperty({
        get syntax(){order.push('syntax');return '<number>';},
        get name(){order.push('name');return '--order';},
        get initialValue(){order.push('initialValue');return '2';},
        get inherits(){order.push('inherits');return false;}
    });
    check(order.join(',') === 'inherits,initialValue,name,syntax', 'Web IDL conversion order');
    const constructed = new CSSStyleSheet();
    constructed.replaceSync('@property --adopted, --second {syntax:"<length>";inherits:false;initial-value:5px}');
    document.adoptedStyleSheets = [constructed];
    check(value('--adopted') === '5px' && value('--second') === '5px', 'adopted multiple-name registration');
    constructed.disabled = true; check(value('--adopted') === '', 'disabled registration removed');
    constructed.disabled = false; check(value('--adopted') === '5px', 'registration restored');
    constructed.replaceSync('@media print {@property --adopted {initial-value:print}} @media all {@property --adopted {initial-value:screen}}');
    check(value('--adopted') === 'screen', 'active conditional registration');
    constructed.deleteRule(1); check(value('--adopted') === '', 'deleted conditional registration');
    const host = document.createElement('div'); body.appendChild(host);
    const shadow = host.attachShadow({mode:'open'});
    shadow.innerHTML = '<style>@property --shadow {syntax:"<number>";inherits:false;initial-value:4}</style><span></span>';
    check(value('--shadow') === '4' && value('--shadow',shadow.querySelector('span')) === '4', 'document-wide shadow registration');
    host.remove(); check(value('--shadow') === '', 'disconnected registration removed');
    const numbers = document.createElement('style');
    numbers.textContent = '@property --n {syntax:"<number>";initial-value:2;inherits:false}'; body.appendChild(numbers);
    c.style.cssText = '--bad-length:var(--n)px';
    check(value('--bad-length') === '2/**/px','substitution preserves token boundaries');
    const urls = new CSSStyleSheet({baseURL:'https://example.test/assets/style.css'});
    urls.replaceSync('@property --url {syntax:"<url>";inherits:false;initial-value:url(initial.png)} #c{--url:url(image.png)}');
    document.adoptedStyleSheets=[urls];
    check(value('--url') === 'url("https://example.test/assets/image.png")','constructable stylesheet base');
    urls.cssRules[1].style.removeProperty('--url');
    check(value('--url') === 'url("https://example.test/assets/initial.png")','initial descriptor stylesheet base');
    CSS.registerProperty({name:'--a:b',syntax:'<number>',initialValue:'5',inherits:false});
    c.style.setProperty('--a:b','7');
    check(value('--a:b') === '7' && c.style.getPropertyValue('--a:b') === '7','CSSOM property name strings');
    const escaped = new CSSStyleSheet();
    escaped.replaceSync('@property --a\\:b {syntax:"<number>"}');
    check(escaped.cssRules[0].name === '--a:b' && escaped.cssRules[0].cssText.startsWith('@property --a\\:b '),'CSSOM identifier escaping');
    const observed = document.adoptedStyleSheets;
    const pushed = new CSSStyleSheet(); pushed.replaceSync('@property --pushed {initial-value:pushed}');
    document.adoptedStyleSheets.push(pushed);
    check(value('--pushed') === 'pushed', 'observable adoption push');
    check(!Reflect.set(observed,String(observed.length + 1),pushed), 'observable adoption prevents holes');
    observed.pop(); check(value('--pushed') === '', 'observable adoption pop');
    document.adoptedStyleSheets = [pushed];
    check(document.adoptedStyleSheets === observed && value('--pushed') === 'pushed', 'observable adoption identity');
    throws(() => observed.push(sheet),'NotAllowedError');
    throws(() => observed.push({}),'TypeError');
    observed.length = 0; check(value('--pushed') === '', 'observable adoption truncation');
    return 'property-registration-ok';
})()
