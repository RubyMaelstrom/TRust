// Reduced official WPT sticky cases linked by Servo #43766. Normative
// authority: CSS Position 3 #stickypos-insets / #stickypos-scroll and CSSOM
// View #dom-element-getboundingclientrect / #dom-htmlelement-offsettop;
// CSSWG local snapshot81c27f686901 (2026-09-06).

#[test]
fn servo_sticky_cssom_rectangles_survive_scroll_style_changes_and_insertion() {
    for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
        let mut engine = platform_engine_with_body();
        engine.set_tier(tier);
        engine.set_tier_threshold(0);
        assert_eq!(string_value(&mut engine, r#"(() => {
            document.body.style.margin='0';
            const style=document.createElement('style');
            style.textContent='.sc{overflow:scroll;width:200px;height:200px}.space{width:2000px;height:2000px}.sticky{position:sticky;top:50px;left:20px;width:100px;height:100px}';
            document.head.append(style);
            const sc=document.createElement('div');sc.className='sc';
            sc.innerHTML='<div class=sticky></div><div class=space></div>';
            document.body.append(sc);
            const sticky=sc.firstElementChild;
            const check=(label,top,left)=>{
                const r=sticky.getBoundingClientRect(),p=sc.getBoundingClientRect();
                if(r.top-p.top!==top||r.left-p.left!==left)throw Error(label+':'+[r.top-p.top,r.left-p.left]);
            };
            check('initial',50,20);
            sc.scrollTop=100;sc.scrollLeft=75;check('scroll',50,20);
            if(sticky.offsetTop!==150||sticky.offsetLeft!==95)throw Error('offset '+[sticky.offsetTop,sticky.offsetLeft]);
            sticky.style.position='relative';check('relative',-50,-55);
            sticky.style.position='sticky';check('sticky again',50,20);
            sc.scrollTop=1500;sc.scrollLeft=500;check('far scroll',50,20);
            sticky.remove();
            const inserted=document.createElement('div');inserted.className='sticky';
            sc.insertBefore(inserted,sc.firstElementChild);
            const r=inserted.getBoundingClientRect(),p=sc.getBoundingClientRect();
            if(r.top-p.top!==50||r.left-p.left!==20)throw Error('inserted '+[r.top-p.top,r.left-p.left]);
            const before=document.createElement('div');before.style.height='65px';sc.insertBefore(before,inserted);
            if(inserted.getBoundingClientRect().top-sc.getBoundingClientRect().top!==50)throw Error('layout change');
            return 'sticky-ok';
        })()"#), "sticky-ok", "{tier:?}");
    }
}

#[test]
fn servo_sticky_viewport_rectangles_include_sticking_but_fixed_descendants_do_not() {
    let mut engine = platform_engine_with_body();
    assert_eq!(string_value(&mut engine, r#"(() => {
        document.body.style.cssText='margin:0;height:3000px';
        const sticky=document.createElement('div');
        sticky.style.cssText='position:sticky;top:10px;width:100px;height:30px';
        const fixed=document.createElement('div');fixed.style.cssText='position:fixed;top:3px;left:4px;width:5px;height:5px';
        const child=document.createElement('div');child.style.cssText='position:sticky;top:4px;width:2px;height:2px';fixed.append(child);
        sticky.append(fixed);document.body.append(sticky);
        window.scrollTo(0,500);
        if(sticky.getBoundingClientRect().top!==10)throw Error('viewport sticky '+sticky.getBoundingClientRect().top);
        if(sticky.offsetTop!==510)throw Error('offsetTop '+sticky.offsetTop);
        if(fixed.getBoundingClientRect().top!==3)throw Error('fixed '+fixed.getBoundingClientRect().top);
        if(child.getBoundingClientRect().top!==4)throw Error('sticky in fixed '+child.getBoundingClientRect().top);
        window.scrollTo(0,0);
        if(sticky.getBoundingClientRect().top!==10)throw Error('initial top');
        return 'viewport-ok';
    })()"#), "viewport-ok");
}

#[test]
fn servo_sticky_overconstrained_both_edges_use_the_scrollable_containing_bounds() {
    let mut engine = platform_engine_with_body();
    assert_eq!(string_value(&mut engine, r#"(() => {
        document.body.style.margin='0';
        const sc=document.createElement('div');
        sc.style.cssText='width:100px;height:100px;overflow:auto;position:relative;display:flex';
        const space=()=>{const e=document.createElement('div');e.style.cssText='width:200px;height:100px;flex:none';return e};
        const sticky=space();sticky.style.cssText+=';position:sticky;left:20px;right:0';
        sc.append(space(),sticky,space());document.body.append(sc);
        for(const [scroll,expected] of [[0,20],[160,180],[220,240],[500,400]]) {
            sc.scrollLeft=scroll;
            if(sticky.offsetLeft!==expected)throw Error(scroll+': '+sticky.offsetLeft+' expected '+expected);
        }
        sc.style.direction='rtl';sticky.style.left='0';sticky.style.right='20px';
        for(const [scroll,expected] of [[0,-120],[-160,-280],[-220,-340],[-500,-500]]) {
            sc.scrollLeft=scroll;
            if(sticky.offsetLeft!==expected)throw Error('rtl '+scroll+': '+sticky.offsetLeft+' expected '+expected);
        }
        return 'edges-ok';
    })()"#), "edges-ok");
}

#[test]
fn servo_sticky_nested_boxes_resolve_after_parent_offsets_on_independent_axes() {
    let mut engine = platform_engine_with_body();
    assert_eq!(string_value(&mut engine, r#"(() => {
        document.body.style.margin='0';
        const style=document.createElement('style');style.textContent=`
            #outer{overflow-y:scroll;overflow-x:clip;width:200px;height:200px}
            #inner{overflow-x:scroll;overflow-y:clip;width:200px;height:1000px}
            #a{position:sticky;top:10px;left:20px;width:100px;height:60px}
            #b{position:sticky;top:25px;left:40px;width:50px;height:20px}`;
        document.head.append(style);
        document.body.innerHTML='<div id=outer><div id=inner><div id=a><div id=b></div></div><div style="width:1000px;height:1000px"></div></div></div>';
        const outer=document.getElementById('outer'),inner=document.getElementById('inner');
        inner.scrollLeft=50;outer.scrollTop=60;
        const o=outer.getBoundingClientRect(),a=document.getElementById('a').getBoundingClientRect(),b=document.getElementById('b').getBoundingClientRect();
        const values=[a.left-o.left,a.top-o.top,b.left-o.left,b.top-o.top];
        if(values.join()!=='20,10,40,25')throw Error(values.join());
        return 'nested-ok';
    })()"#), "nested-ok");
}
