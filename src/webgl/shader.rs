//! GLSL ES 1.00 preparation in Rust. The system GLES compiler performs GLSL
//! type checking and code generation; this pass enforces WebGL-specific language
//! restrictions before the driver sees the source, and initializes variables.
//! WebGL §§4.3, 4.5, 6.14–6.18, 6.31; GLSL ES 1.00 Appendix A §§4–5.

use glsl_lang::{
    ast,
    lexer::{HasLexerError, LangLexer, LangLexerIterator, Token},
    parse::{DefaultParse, HasParser, ParseContext, ParseOptions},
    transpiler::glsl::{FormattingState, show_translation_unit},
    visitor::{Host, Visit, Visitor},
};
use std::collections::{HashMap, HashSet};

const MAX_SOURCE: usize = 1024 * 1024;
const MAX_NODES: usize = 100_000;

// Bound preprocessed input before constructing recursive expression trees.
// Limits are implementation resources, not alternate GLSL language rules.
struct Limited<I> {
    inner: I,
    tokens: usize,
    depth: usize,
    expression: usize,
    exceeded: bool,
}
impl<I: HasLexerError> HasLexerError for Limited<I> {
    type Error = I::Error;
}
impl<I: LangLexerIterator> Iterator for Limited<I> {
    type Item = <I as Iterator>::Item;
    fn next(&mut self) -> Option<Self::Item> {
        if self.exceeded {
            return None;
        }
        let item = self.inner.next()?;
        self.tokens += 1;
        self.expression += 1;
        if let Ok((_, token, _)) = &item {
            match token {
                Token::LeftParen | Token::LeftBracket | Token::LeftBrace => self.depth += 1,
                Token::RightParen | Token::RightBracket | Token::RightBrace => {
                    self.depth = self.depth.saturating_sub(1)
                }
                _ => {}
            }
            if matches!(
                token,
                Token::Semicolon | Token::LeftBrace | Token::RightBrace
            ) {
                self.expression = 0;
            }
        }
        if self.tokens > MAX_NODES || self.depth > 64 || self.expression > 1024 {
            self.exceeded = true;
            None
        } else {
            Some(item)
        }
    }
}

// Shader preprocessing must never have access to local files, even if a
// non-WebGL include extension appears in an untrusted source.
#[derive(Default)]
struct NoFiles;
impl glsl_lang_pp::processor::fs::FileSystem for NoFiles {
    type Error = std::io::Error;
    fn canonicalize(&self, _: &std::path::Path) -> Result<std::path::PathBuf, Self::Error> {
        Err(std::io::ErrorKind::PermissionDenied.into())
    }
    fn exists(&self, _: &std::path::Path) -> bool {
        false
    }
    fn read(&self, _: &std::path::Path) -> Result<std::borrow::Cow<'_, str>, Self::Error> {
        Err(std::io::ErrorKind::PermissionDenied.into())
    }
}

fn versioned(source: &str) -> String {
    let mut start = source;
    loop {
        start = start.trim_start();
        if let Some(comment) = start.strip_prefix("//") {
            start = comment.split_once('\n').map_or("", |(_, s)| s);
        } else if let Some(comment) = start.strip_prefix("/*") {
            start = comment.split_once("*/").map_or("", |(_, s)| s);
        } else {
            break;
        }
    }
    // The preprocessor's desktop default is 110; GLSL ES's default is 100.
    if start
        .strip_prefix('#')
        .is_some_and(|s| s.trim_start().starts_with("version"))
    {
        source.into()
    } else {
        format!("#version 100\n#line 1\n{source}")
    }
}

pub(super) fn prepare(
    source: &str,
    vertex: bool,
    derivatives: bool,
    highp: bool,
) -> Result<String, String> {
    if source.len() > MAX_SOURCE {
        return Err("Shader source exceeds the resource limit".into());
    }
    let options = ParseOptions {
        default_version: 100,
        ..Default::default()
    };
    use glsl_lang::lexer::full::fs::PreprocessorExt;
    use glsl_lang_pp::{
        exts::{ExtensionSpec, Registry},
        processor::{
            ProcessorState,
            fs::Processor,
            nodes::{Define, DefineObject},
        },
    };
    let mut registry = Registry::new();
    if derivatives {
        registry.add(ExtensionSpec::new(
            "GL_OES_standard_derivatives".into(),
            vec![],
        ));
    }
    let mut state = ProcessorState::builder()
        .registry(&registry)
        .core_profile(false)
        .definition(Define::object("GL_ES".into(), DefineObject::one(), true));
    if highp {
        state = state.definition(Define::object(
            "GL_FRAGMENT_PRECISION_HIGH".into(),
            DefineObject::one(),
            true,
        ));
    }
    let state = state.finish();
    let mut processor = Processor::new_with_fs(NoFiles);
    let source = versioned(source);
    let file = processor
        .open_source(&source, "")
        .with_state(state)
        .with_registry(&registry);
    let context = ParseContext::default();
    let mut lexer = Limited {
        inner: glsl_lang::lexer::full::fs::Lexer::new(file, &options).run(context.clone()),
        tokens: 0,
        depth: 0,
        expression: 0,
        exceeded: false,
    };
    let parser = <ast::TranslationUnit as HasParser>::Parser::new();
    let parsed = glsl_lang::parse::LangParser::parse(&parser, context, &mut lexer);
    if lexer.exceeded {
        return Err("Shader exceeds parser resource limits".into());
    }
    let mut unit = parsed.map_err(|e| format!("GLSL: {e:?}"))?;
    lexer.inner.into_directives().inject(&mut unit);
    let mut validator = Validator {
        error: None,
        nodes: 0,
        derivatives,
    };
    unit.visit(&mut validator);
    if let Some(error) = validator.error {
        return Err(error);
    }
    let mut walk = ScopeCheck::new(vertex);
    for declaration in &unit.0 {
        let prototype = match &declaration.content {
            ast::ExternalDeclarationData::FunctionDefinition(f) => Some(&f.prototype),
            ast::ExternalDeclarationData::Declaration(d) => {
                if let ast::DeclarationData::FunctionPrototype(f) = &d.content {
                    Some(f)
                } else {
                    None
                }
            }
            _ => None,
        };
        if let Some(f) = prototype {
            let writable=f.parameters.iter().map(|p| {
                let qualifier=match &p.content {ast::FunctionParameterDeclarationData::Named(q,_)|ast::FunctionParameterDeclarationData::Unnamed(q,_)=>q};
                qualifier.as_ref().is_some_and(|q|q.qualifiers.iter().any(|q|matches!(&q.content,ast::TypeQualifierSpecData::Storage(s) if matches!(s.content,ast::StorageQualifierData::Out|ast::StorageQualifierData::InOut))))
            }).collect();
            walk.functions
                .entry(f.name.as_str().into())
                .or_default()
                .push(writable);
        }
    }
    for declaration in &mut unit.0 {
        match &mut declaration.content {
            ast::ExternalDeclarationData::Declaration(d) => {
                walk.declaration(d, true)?;
            }
            ast::ExternalDeclarationData::FunctionDefinition(f) => {
                walk.scopes.push(HashMap::new());
                let mut output_initializers = vec![];
                for p in &f.prototype.parameters {
                    if let ast::FunctionParameterDeclarationData::Named(q, d) = &p.content {
                        walk.insert(
                            d.ident.ident.as_str(),
                            &d.ty,
                            d.ident.array_spec.as_ref(),
                            false,
                            false,
                        );
                        if q.as_ref().is_some_and(|q|q.qualifiers.iter().any(|q|matches!(&q.content,ast::TypeQualifierSpecData::Storage(s) if s.content==ast::StorageQualifierData::Out))) {
                            walk.zero(d.ident.ident.as_str(),&d.ty,d.ident.array_spec.as_ref(),0,&mut output_initializers)?;
                        }
                    }
                }
                walk.compound(&mut f.statement)?;
                if !output_initializers.is_empty() {
                    let mut prefix = statements(&output_initializers.join("\n"))?;
                    prefix.append(&mut f.statement.statement_list);
                    f.statement.statement_list = prefix;
                }
                walk.scopes.pop();
            }
            _ => {}
        }
    }
    // A helper defined after all global declarations preserves lexical scope
    // even when a valid unused global is declared after main's definition.
    if !walk.global_initializers.is_empty() {
        let mut prototype = ast::TranslationUnit::parse("void _webgl_initialize_globals();")
            .map_err(|e| e.to_string())?;
        let helper = ast::TranslationUnit::parse(&format!(
            "void _webgl_initialize_globals(){{{}}}",
            walk.global_initializers.join("\n")
        ))
        .map_err(|e| e.to_string())?;
        let index = unit
            .0
            .iter()
            .position(|d| {
                matches!(
                    d.content,
                    ast::ExternalDeclarationData::FunctionDefinition(_)
                )
            })
            .unwrap_or(unit.0.len());
        unit.0.insert(index, prototype.0.remove(0));
        unit.0.extend(helper.0);
    }
    for declaration in &mut unit.0 {
        if let ast::ExternalDeclarationData::FunctionDefinition(f) = &mut declaration.content
            && f.prototype.name.as_str() == "main"
        {
            let mut prefix = String::new();
            if vertex {
                prefix.push_str("gl_Position=vec4(0.0);");
            }
            if !walk.global_initializers.is_empty() {
                prefix.push_str("_webgl_initialize_globals();");
            }
            let mut init = statements(&prefix)?;
            init.append(&mut f.statement.statement_list);
            f.statement.statement_list = init;
        }
    }
    let mut source = String::new();
    show_translation_unit(&mut source, &unit, FormattingState::default())
        .map_err(|e| e.to_string())?;
    // Only normalized, preprocessed ASCII reaches the system driver.
    if !source.is_ascii() {
        return Err("Characters outside the GLSL source character set".into());
    }
    Ok(source)
}

#[derive(Default)]
struct Validator {
    error: Option<String>,
    nodes: usize,
    derivatives: bool,
}
impl Validator {
    fn reject(&mut self, message: &str) {
        if self.error.is_none() {
            self.error = Some(message.into());
        }
    }
}
impl Visitor for Validator {
    fn visit_identifier(&mut self, id: &ast::Identifier) -> Visit {
        let id = id.as_str();
        if id.len() > 256 {
            self.reject("GLSL tokens may not exceed 256 characters");
        }
        if id.starts_with("webgl_") || id.starts_with("_webgl_") {
            self.reject("Reserved WebGL identifier");
        }
        if !self.derivatives && matches!(id, "dFdx" | "dFdy" | "fwidth") {
            self.reject("OES_standard_derivatives is not enabled");
        }
        Visit::Children
    }
    fn visit_preprocessor(&mut self, p: &ast::Preprocessor) -> Visit {
        match &p.content {
            ast::PreprocessorData::Version(v) if v.version != 100 || v.profile.is_some() => {
                self.reject("WebGL 1 requires GLSL ES 1.00")
            }
            ast::PreprocessorData::Extension(e) => {
                if let ast::PreprocessorExtensionNameData::Specific(name) = &e.name.content
                    && (name.as_str() != "GL_OES_standard_derivatives" || !self.derivatives)
                    && !matches!(
                        e.behavior.as_ref().map(|b| &b.content),
                        Some(ast::PreprocessorExtensionBehaviorData::Disable)
                    )
                {
                    self.reject("Shader extension is not enabled for this WebGL context");
                }
            }
            _ => {}
        }
        Visit::Children
    }
    fn visit_type_specifier_non_array(&mut self, ty: &ast::TypeSpecifierNonArray) -> Visit {
        use ast::TypeSpecifierNonArrayData::*;
        if !matches!(
            ty.content,
            Void | Bool
                | Int
                | Float
                | Vec2
                | Vec3
                | Vec4
                | BVec2
                | BVec3
                | BVec4
                | IVec2
                | IVec3
                | IVec4
                | Mat2
                | Mat3
                | Mat4
                | Sampler2D
                | SamplerCube
                | Struct(_)
                | TypeName(_)
        ) {
            self.reject("Type is not part of GLSL ES 1.00");
        }
        Visit::Children
    }
    fn visit_statement(&mut self, s: &ast::Statement) -> Visit {
        if matches!(
            s.content,
            ast::StatementData::Switch(_) | ast::StatementData::CaseLabel(_)
        ) {
            self.reject("Switch is not part of GLSL ES 1.00");
        }
        Visit::Children
    }
    fn visit_expr(&mut self, expr: &ast::Expr) -> Visit {
        self.nodes += 1;
        if self.nodes > MAX_NODES {
            self.reject("Shader expression budget exceeded");
            return Visit::Parent;
        }
        if matches!(
            expr.content,
            ast::ExprData::UIntConst(_) | ast::ExprData::DoubleConst(_)
        ) {
            self.reject("Literal is not part of GLSL ES 1.00");
        }
        Visit::Children
    }
}

#[derive(Clone)]
struct Variable {
    constant: bool,
    uniform: bool,
    ty: ast::TypeSpecifier,
    array: Option<ast::ArraySpecifier>,
}
struct ScopeCheck {
    vertex: bool,
    scopes: Vec<HashMap<String, Variable>>,
    loops: HashSet<String>,
    global_initializers: Vec<String>,
    structs: HashMap<String, ast::StructSpecifier>,
    functions: HashMap<String, Vec<Vec<bool>>>,
}
impl ScopeCheck {
    fn new(vertex: bool) -> Self {
        Self {
            vertex,
            scopes: vec![HashMap::new()],
            loops: HashSet::new(),
            global_initializers: vec![],
            structs: HashMap::new(),
            functions: HashMap::new(),
        }
    }
    fn insert(
        &mut self,
        name: &str,
        ty: &ast::TypeSpecifier,
        array: Option<&ast::ArraySpecifier>,
        constant: bool,
        uniform: bool,
    ) {
        self.scopes.last_mut().unwrap().insert(
            name.into(),
            Variable {
                constant,
                uniform,
                ty: ty.clone(),
                array: array.cloned(),
            },
        );
    }
    fn variable(&self, name: &str) -> Option<&Variable> {
        self.scopes.iter().rev().find_map(|s| s.get(name))
    }
    fn constant(&self, expr: &ast::Expr, index: bool) -> bool {
        use ast::ExprData::*;
        match &expr.content {
            IntConst(_) | FloatConst(_) | BoolConst(_) => true,
            Variable(n) => {
                self.variable(n.as_str()).is_some_and(|v| v.constant)
                    || (index && self.loops.contains(n.as_str()))
                    || n.as_str().starts_with("gl_Max")
            }
            Unary(op, e) => {
                matches!(
                    op.content,
                    ast::UnaryOpData::Add | ast::UnaryOpData::Minus | ast::UnaryOpData::Not
                ) && self.constant(e, index)
            }
            Binary(_, a, b) | Bracket(a, b) => self.constant(a, index) && self.constant(b, index),
            Ternary(a, b, c) => {
                self.constant(a, index) && self.constant(b, index) && self.constant(c, index)
            }
            FunCall(f, args) => {
                constant_function(f) && args.iter().all(|a| self.constant(a, index))
            }
            Dot(e, _) => self.constant(e, index),
            _ => false,
        }
    }
    fn declaration(
        &mut self,
        d: &mut ast::Declaration,
        global: bool,
    ) -> Result<Vec<ast::Statement>, String> {
        let ast::DeclarationData::InitDeclaratorList(list) = &mut d.content else {
            return Ok(vec![]);
        };
        if let ast::TypeSpecifierNonArrayData::Struct(s) = &list.head.ty.ty.ty.content
            && let Some(name) = &s.name
        {
            self.structs.insert(name.as_str().into(), s.clone());
        }
        let constant = storage(&list.head.ty, ast::StorageQualifierData::Const);
        let uniform = storage(&list.head.ty, ast::StorageQualifierData::Uniform);
        let attribute = storage(&list.head.ty, ast::StorageQualifierData::Attribute);
        let varying = storage(&list.head.ty, ast::StorageQualifierData::Varying);
        let initialize = !constant && !uniform && !attribute && !(varying && !self.vertex);
        let ty = list.head.ty.ty.clone();
        let mut prefix = vec![];
        let mut vars = Vec::new();
        let ast::InitDeclaratorListData { head, tail } = &mut list.content;
        if let Some(name) = &head.name {
            vars.push((
                name.clone(),
                head.array_specifier.clone(),
                &mut head.initializer,
            ));
        }
        for tail in tail {
            vars.push((
                tail.ident.ident.clone(),
                tail.ident.array_spec.clone(),
                &mut tail.initializer,
            ));
        }
        for (name, array, init) in vars {
            self.insert(name.as_str(), &ty, array.as_ref(), constant, uniform);
            if let Some(initializer) = init.as_ref()
                && let ast::InitializerData::Simple(expr) = &initializer.content
            {
                self.expression(expr)?;
                if global && initialize && !self.global_expression(expr) {
                    return Err("Invalid global initializer expression".into());
                }
            }
            if initialize {
                let mut zeroes = vec![];
                self.zero(name.as_str(), &ty, array.as_ref(), 0, &mut zeroes)?;
                if let Some(initializer) = init.take() {
                    if let ast::InitializerData::Simple(expr) = &initializer.content {
                        let mut expression = String::new();
                        glsl_lang::transpiler::glsl::show_expr(
                            &mut expression,
                            expr,
                            &mut FormattingState::default(),
                        )
                        .map_err(|e| e.to_string())?;
                        zeroes.push(format!("{}={expression};", name.as_str()));
                    } else {
                        return Err("GLSL ES 1.00 does not support aggregate initializers".into());
                    }
                }
                if global {
                    self.global_initializers.extend(zeroes);
                } else {
                    prefix.extend(statements(&zeroes.join("\n"))?);
                }
            }
        }
        Ok(prefix)
    }
    fn zero(
        &self,
        name: &str,
        ty: &ast::TypeSpecifier,
        array: Option<&ast::ArraySpecifier>,
        depth: usize,
        output: &mut Vec<String>,
    ) -> Result<(), String> {
        if depth > 4 {
            return Err("WebGL structures may nest at most four levels".into());
        }
        if let Some(array) = array {
            let Some(dimension) = array.dimensions.first() else {
                return Err("Missing array dimension".into());
            };
            let ast::ArraySpecifierDimensionData::ExplicitlySized(size) = &dimension.content else {
                return Err("Unsized array".into());
            };
            // Use a bounded initialization loop, so array size expressions are
            // still evaluated by the GLSL compiler with their correct type.
            let mut text = String::new();
            glsl_lang::transpiler::glsl::show_expr(
                &mut text,
                size,
                &mut FormattingState::default(),
            )
            .map_err(|e| e.to_string())?;
            let index = format!("_webgl_init_{depth}");
            output.push(format!("for(int {index}=0;{index}<({text});{index}++){{"));
            self.zero(&format!("{name}[{index}]"), ty, None, depth, output)?;
            output.push("}".into());
            return Ok(());
        }
        use ast::TypeSpecifierNonArrayData::*;
        let structure = match &ty.ty.content {
            Struct(s) => Some(s),
            TypeName(n) => self.structs.get(n.as_str()),
            _ => None,
        };
        if let Some(s) = structure {
            for field in &s.fields {
                for id in &field.identifiers {
                    self.zero(
                        &format!("{name}.{}", id.ident.as_str()),
                        &field.ty,
                        id.array_spec.as_ref(),
                        depth + 1,
                        output,
                    )?;
                }
            }
        } else {
            let value = match ty.ty.content {
                Bool => "false",
                Int => "0",
                Float => "0.0",
                Vec2 => "vec2(0.0)",
                Vec3 => "vec3(0.0)",
                Vec4 => "vec4(0.0)",
                IVec2 => "ivec2(0)",
                IVec3 => "ivec3(0)",
                IVec4 => "ivec4(0)",
                BVec2 => "bvec2(false)",
                BVec3 => "bvec3(false)",
                BVec4 => "bvec4(false)",
                Mat2 => "mat2(0.0)",
                Mat3 => "mat3(0.0)",
                Mat4 => "mat4(0.0)",
                _ => return Ok(()),
            };
            output.push(format!("{name}={value};"));
        }
        Ok(())
    }
    fn compound(&mut self, block: &mut ast::CompoundStatement) -> Result<(), String> {
        self.scopes.push(HashMap::new());
        let mut out = Vec::new();
        for statement in std::mem::take(&mut block.statement_list) {
            out.extend(self.expanded(statement)?);
        }
        block.statement_list = out;
        self.scopes.pop();
        Ok(())
    }
    fn expanded(&mut self, mut statement: ast::Statement) -> Result<Vec<ast::Statement>, String> {
        let mut rest = Vec::new();
        if let ast::StatementData::Declaration(d) = &mut statement.content
            && let ast::DeclarationData::InitDeclaratorList(list) = &mut d.content
        {
            for tail in std::mem::take(&mut list.tail) {
                let mut head = list.head.clone();
                head.name = Some(tail.ident.ident.clone());
                head.array_specifier = tail.ident.array_spec.clone();
                head.initializer = tail.initializer.clone();
                rest.push(
                    ast::StatementData::Declaration(
                        ast::DeclarationData::InitDeclaratorList(
                            ast::InitDeclaratorListData { head, tail: vec![] }.into(),
                        )
                        .into(),
                    )
                    .into(),
                );
            }
        }
        let mut out = vec![];
        for mut statement in std::iter::once(statement).chain(rest) {
            let extra = self.statement(&mut statement)?;
            out.push(statement);
            out.extend(extra);
        }
        Ok(out)
    }
    fn substatement(&mut self, s: &mut ast::Statement) -> Result<(), String> {
        let placeholder = ast::StatementData::Compound(
            ast::CompoundStatementData {
                statement_list: vec![],
            }
            .into(),
        )
        .into();
        let mut expanded = self.expanded(std::mem::replace(s, placeholder))?;
        *s = if expanded.len() == 1 {
            expanded.pop().unwrap()
        } else {
            ast::StatementData::Compound(
                ast::CompoundStatementData {
                    statement_list: expanded,
                }
                .into(),
            )
            .into()
        };
        Ok(())
    }
    fn global_expression(&self, expr: &ast::Expr) -> bool {
        use ast::ExprData::*;
        match &expr.content {
            IntConst(_) | FloatConst(_) | BoolConst(_) => true,
            Variable(n) => self.variable(n.as_str()).is_some() || n.as_str().starts_with("gl_Max"),
            Unary(op, e) => {
                matches!(
                    op.content,
                    ast::UnaryOpData::Add | ast::UnaryOpData::Minus | ast::UnaryOpData::Not
                ) && self.global_expression(e)
            }
            Binary(_, a, b) | Bracket(a, b) => {
                self.global_expression(a) && self.global_expression(b)
            }
            Ternary(a, b, c) => {
                self.global_expression(a) && self.global_expression(b) && self.global_expression(c)
            }
            Dot(e, _) => self.global_expression(e),
            FunCall(f, args) => {
                constant_function(f) && args.iter().all(|e| self.global_expression(e))
            }
            _ => false,
        }
    }
    fn statement(&mut self, s: &mut ast::Statement) -> Result<Vec<ast::Statement>, String> {
        use ast::StatementData::*;
        match &mut s.content {
            Declaration(d) => return self.declaration(d, false),
            Compound(c) => self.compound(c)?,
            Expression(e) => {
                if let Some(e) = &e.0 {
                    self.expression(e)?;
                }
            }
            Selection(s) => {
                self.expression(&s.cond)?;
                match &mut s.rest.content {
                    ast::SelectionRestStatementData::Statement(s) => {
                        self.substatement(s)?;
                    }
                    ast::SelectionRestStatementData::Else(a, b) => {
                        self.substatement(a)?;
                        self.substatement(b)?;
                    }
                }
            }
            Iteration(i) => {
                let ast::IterationStatementData::For(init, rest, body) = &mut i.content else {
                    return Err("WebGL 1 permits only Appendix A for loops".into());
                };
                self.scopes.push(HashMap::new());
                let ast::ForInitStatementData::Declaration(d) = &mut init.content else {
                    return Err("Loop index must be declared in its initializer".into());
                };
                let ast::DeclarationData::InitDeclaratorList(list) = &d.content else {
                    return Err("Invalid loop declaration".into());
                };
                if !list.tail.is_empty()
                    || !matches!(
                        list.head.ty.ty.ty.content,
                        ast::TypeSpecifierNonArrayData::Int | ast::TypeSpecifierNonArrayData::Float
                    )
                {
                    return Err("Loop requires one int or float index".into());
                }
                let name = list
                    .head
                    .name
                    .as_ref()
                    .ok_or("Missing loop index")?
                    .as_str()
                    .to_owned();
                let Some(initial) = &list.head.initializer else {
                    return Err("Loop index requires a constant initializer".into());
                };
                if !matches!(&initial.content, ast::InitializerData::Simple(e) if self.constant(e,false))
                {
                    return Err("Loop initializer must be constant".into());
                }
                let Some(condition) = &rest.condition else {
                    return Err("Loop requires a condition".into());
                };
                let ast::ConditionData::Expr(condition) = &condition.content else {
                    return Err("Invalid loop condition".into());
                };
                if !matches!(&condition.content, ast::ExprData::Binary(op,l,r) if matches!(op.content,ast::BinaryOpData::Lt|ast::BinaryOpData::Lte|ast::BinaryOpData::Gt|ast::BinaryOpData::Gte|ast::BinaryOpData::Equal|ast::BinaryOpData::NonEqual) && variable_name(l)==Some(&name) && self.constant(r,false))
                {
                    return Err("Loop condition must compare its index with a constant".into());
                }
                let valid_increment = rest.post_expr.as_ref().is_some_and(|p| match &p.content {
                    ast::ExprData::PostInc(e) | ast::ExprData::PostDec(e) => {
                        variable_name(e) == Some(&name)
                    }
                    ast::ExprData::Assignment(e, op, r) => {
                        variable_name(e) == Some(&name)
                            && matches!(
                                op.content,
                                ast::AssignmentOpData::Add | ast::AssignmentOpData::Sub
                            )
                            && self.constant(r, false)
                    }
                    _ => false,
                });
                if !valid_increment {
                    return Err("Invalid Appendix A loop increment".into());
                }
                let ast::DeclarationData::InitDeclaratorList(list) = &d.content else {
                    unreachable!()
                };
                self.insert(&name, &list.head.ty.ty, None, false, false);
                let shadowed = self.loops.contains(&name);
                self.loops.insert(name.clone());
                self.substatement(body)?;
                if !shadowed {
                    self.loops.remove(&name);
                }
                self.scopes.pop();
            }
            Jump(j) => {
                if let ast::JumpStatementData::Return(Some(e)) = &j.content {
                    self.expression(e)?;
                }
            }
            _ => {}
        }
        Ok(vec![])
    }
    fn expression(&self, expression: &ast::Expr) -> Result<(), String> {
        struct Check<'a> {
            scope: &'a ScopeCheck,
            error: Option<String>,
        }
        impl Visitor for Check<'_> {
            fn visit_expr(&mut self, e: &ast::Expr) -> Visit {
                use ast::ExprData::*;
                match &e.content {
                    Bracket(base, index) if !self.scope.constant(index, true) => {
                        let allowed = self.scope.vertex
                            && variable_name(base)
                                .and_then(|n| self.scope.variable(n))
                                .is_some_and(|v| {
                                    v.uniform
                                        && !matches!(
                                            v.ty.ty.content,
                                            ast::TypeSpecifierNonArrayData::Sampler2D
                                                | ast::TypeSpecifierNonArrayData::SamplerCube
                                        )
                                });
                        if !allowed {
                            self.error =
                                Some("Array indexing must use a constant-index-expression".into());
                        }
                    }
                    Bracket(base, index) => {
                        if let (Some(v), IntConst(i)) = (
                            variable_name(base).and_then(|n| self.scope.variable(n)),
                            &index.content,
                        ) && let Some(array) = &v.array
                            && let Some(d) = array.dimensions.first()
                            && let ast::ArraySpecifierDimensionData::ExplicitlySized(size) =
                                &d.content
                            && let IntConst(n) = size.content
                            && (*i < 0 || *i >= n)
                        {
                            self.error = Some("Constant array index is out of bounds".into());
                        }
                    }
                    FunCall(function, args) => {
                        if let Some(overloads) = function
                            .as_ident()
                            .and_then(|name| self.scope.functions.get(name.as_str()))
                        {
                            for (index, arg) in args.iter().enumerate() {
                                if variable_name(arg).is_some_and(|n| self.scope.loops.contains(n))
                                    && overloads.iter().any(|p| p.get(index) == Some(&true))
                                {
                                    self.error = Some(
                                        "Loop index cannot be an out or inout argument".into(),
                                    );
                                }
                            }
                        }
                    }
                    Assignment(base, _, _) | PostInc(base) | PostDec(base)
                        if variable_name(base).is_some_and(|n| self.scope.loops.contains(n)) =>
                    {
                        self.error = Some("Loop index is modified inside its body".into())
                    }
                    Unary(op, base)
                        if matches!(op.content, ast::UnaryOpData::Inc | ast::UnaryOpData::Dec)
                            && variable_name(base)
                                .is_some_and(|n| self.scope.loops.contains(n)) =>
                    {
                        self.error = Some("Loop index is modified inside its body".into())
                    }
                    _ => {}
                }
                Visit::Children
            }
        }
        let mut check = Check {
            scope: self,
            error: None,
        };
        expression.visit(&mut check);
        check.error.map_or(Ok(()), Err)
    }
}
fn constant_function(function: &ast::FunIdentifier) -> bool {
    if matches!(function.content, ast::FunIdentifierData::TypeSpecifier(_)) {
        return true;
    }
    function.as_ident().is_some_and(|n| {
        matches!(
            n.as_str(),
            "radians"
                | "degrees"
                | "sin"
                | "cos"
                | "tan"
                | "asin"
                | "acos"
                | "atan"
                | "pow"
                | "exp"
                | "log"
                | "exp2"
                | "log2"
                | "sqrt"
                | "inversesqrt"
                | "abs"
                | "sign"
                | "floor"
                | "ceil"
                | "fract"
                | "mod"
                | "min"
                | "max"
                | "clamp"
                | "mix"
                | "step"
                | "smoothstep"
                | "length"
                | "distance"
                | "dot"
                | "cross"
                | "normalize"
                | "faceforward"
                | "reflect"
                | "refract"
                | "matrixCompMult"
                | "lessThan"
                | "lessThanEqual"
                | "greaterThan"
                | "greaterThanEqual"
                | "equal"
                | "notEqual"
                | "any"
                | "all"
                | "not"
        )
    })
}
fn storage(ty: &ast::FullySpecifiedType, storage: ast::StorageQualifierData) -> bool {
    ty.qualifier.as_ref().is_some_and(|q| {
        q.qualifiers.iter().any(
            |q| matches!(&q.content,ast::TypeQualifierSpecData::Storage(s) if s.content==storage),
        )
    })
}
fn variable_name(e: &ast::Expr) -> Option<&str> {
    if let ast::ExprData::Variable(n) = &e.content {
        Some(n.as_str())
    } else {
        None
    }
}
fn statements(source: &str) -> Result<Vec<ast::Statement>, String> {
    let unit = ast::TranslationUnit::parse(&format!("void main(){{{source}}}"))
        .map_err(|e| e.to_string())?;
    match unit.0.into_iter().next().unwrap().content {
        ast::ExternalDeclarationData::FunctionDefinition(f) => {
            Ok(f.content.statement.content.statement_list)
        }
        _ => unreachable!(),
    }
}

#[cfg(test)]
mod tests {
    fn prepare(source: &str, vertex: bool, derivatives: bool) -> Result<String, String> {
        super::prepare(source, vertex, derivatives, true)
    }
    #[test]
    fn webgl_shader_macro_expansion_is_bounded_before_tokenization() {
        let mut source = String::from("#define M0 1.0\n");
        for i in 1..25 {
            source.push_str(&format!("#define M{i} M{} + M{}\n", i - 1, i - 1));
        }
        source.push_str("void main(){gl_Position=vec4(M24);}");
        let error = prepare(&source, true, false).unwrap_err();
        assert!(
            error.contains("resource limit") || error.contains("MacroExpansionLimit"),
            "{error}"
        );
        let repeated = format!(
            "#define SUM(x) ({})\nvoid main(){{gl_Position=vec4(SUM({}));}}",
            vec!["x"; 512].join("+"),
            vec!["1.0"; 1024].join("+")
        );
        let error = prepare(&repeated, true, false).unwrap_err();
        assert!(error.contains("MacroExpansionLimit"), "{error}");
        let conditional = format!(
            "#if {}1{}\nvoid main(){{}}\n#endif",
            "(".repeat(100),
            ")".repeat(100)
        );
        assert!(prepare(&conditional, true, false).is_err());
        assert!(
            prepare(
                "#define TWICE(x) ((x)+(x))\nvoid main(){gl_Position=vec4(TWICE(1.0));}",
                true,
                false
            )
            .is_ok()
        );
    }
    #[test]
    fn webgl_shader_parser_bounds_and_precision() {
        assert!(
            prepare(
                &format!(
                    "void main(){{gl_Position=vec4({}0.{});}}",
                    "(".repeat(100),
                    ")".repeat(100)
                ),
                true,
                false
            )
            .unwrap_err()
            .contains("resource")
        );
        assert!(
            prepare(
                &format!("void main(){{float x={}0.;}}", "0.+".repeat(2000)),
                true,
                false
            )
            .unwrap_err()
            .contains("resource")
        );
        assert!(super::prepare("#ifdef GL_FRAGMENT_PRECISION_HIGH\n#error highp is unavailable\n#endif\nvoid main(){}",false,false,false).is_ok());
    }
    #[test]
    fn webgl_shader_rules_apply_after_macro_expansion() {
        assert!(prepare("#if !defined(GL_ES) || __VERSION__ != 100\n#error Incorrect ES environment\n#endif\nvoid main(){}",true,false).is_ok());
        assert!(prepare("#ifdef GL_EXT_shader_texture_lod\n#error Extension was not enabled\n#endif\nvoid main(){}",true,false).is_ok());
        assert!(prepare("#define LOOP while\nvoid main(){LOOP(true){}}", true, false).is_err());
        assert!(prepare("#version 300 es\nvoid main(){}", true, false).is_err());
        assert!(
            prepare(
                "// \u{1f600}\nvoid main(){gl_Position=vec4(1.);}",
                true,
                false
            )
            .is_ok()
        );
        assert!(prepare("void main(){float webgl_private=1.;}", true, false).is_err());
        assert!(prepare("void main(){for(int i=0;i<4;i++){i++;}}", true, false).is_err());
        assert!(
            prepare(
                "void change(inout int i){i--;} void main(){for(int i=0;i<4;i++){change(i);}}",
                true,
                false
            )
            .is_err()
        );
        assert!(
            prepare(
                "void main(){for(int i=0;i<4;i++){gl_Position+=vec4(float(i));}}",
                true,
                false
            )
            .is_ok()
        );
    }
    #[test]
    fn webgl_shader_initializes_private_storage() {
        let s = prepare(
            "float a;void main(){vec3 v;float x[2];gl_Position=vec4(v,a);}",
            true,
            false,
        )
        .unwrap();
        assert!(s.contains("a = 0."));
        assert!(s.contains("v = vec3(0."));
        assert!(s.contains("_webgl_init_0"));
    }
}
