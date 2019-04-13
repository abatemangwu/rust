use crate::table::PerDefTable;

use rustc::hir;
use rustc::hir::def::{self, CtorKind};
use rustc::hir::def_id::{DefIndex, DefId};
use rustc::middle::exported_symbols::{ExportedSymbol, SymbolExportLevel};
use rustc::middle::cstore::{DepKind, LinkagePreference, NativeLibrary, ForeignModule};
use rustc::middle::lang_items;
use rustc::mir;
use rustc::session::CrateDisambiguator;
use rustc::ty::{self, Ty, ReprOptions};
use rustc_target::spec::{PanicStrategy, TargetTriple};
use rustc_data_structures::svh::Svh;

use rustc_serialize::{self as serialize, Encodable};
use syntax::{ast, attr};
use syntax::edition::Edition;
use syntax::symbol::Symbol;
use syntax_pos::{self, Span};

use std::marker::PhantomData;
use std::num::NonZeroUsize;

pub fn rustc_version() -> String {
    format!("rustc {}",
            option_env!("CFG_VERSION").unwrap_or("unknown version"))
}

/// Metadata encoding version.
/// N.B., increment this if you change the format of metadata such that
/// the rustc version can't be found to compare with `rustc_version()`.
pub const METADATA_VERSION: u8 = 4;

/// Metadata header which includes `METADATA_VERSION`.
/// To get older versions of rustc to ignore this metadata,
/// there are 4 zero bytes at the start, which are treated
/// as a length of 0 by old compilers.
///
/// This header is followed by the position of the `CrateRoot`,
/// which is encoded as a 32-bit big-endian unsigned integer,
/// and further followed by the rustc version string.
pub const METADATA_HEADER: &[u8; 12] =
    &[0, 0, 0, 0, b'r', b'u', b's', b't', 0, 0, 0, METADATA_VERSION];

/// Additional metadata for a `Lazy<T>` where `T` may not be `Sized`,
/// e.g. for `Lazy<[T]>`, this is the length (count of `T` values).
pub trait LazyMeta {
    type Meta: Copy + 'static;

    /// Returns the minimum encoded size.
    // FIXME(eddyb) Give better estimates for certain types.
    fn min_size(meta: Self::Meta) -> usize;
}

impl<T: Encodable> LazyMeta for T {
    type Meta = ();

    fn min_size(_: ()) -> usize {
        assert_ne!(std::mem::size_of::<T>(), 0);
        1
    }
}

impl<T: Encodable> LazyMeta for [T] {
    type Meta = usize;

    fn min_size(len: usize) -> usize {
        len * T::min_size(())
    }
}

/// A value of type T referred to by its absolute position
/// in the metadata, and which can be decoded lazily.
///
/// Metadata is effective a tree, encoded in post-order,
/// and with the root's position written next to the header.
/// That means every single `Lazy` points to some previous
/// location in the metadata and is part of a larger node.
///
/// The first `Lazy` in a node is encoded as the backwards
/// distance from the position where the containing node
/// starts and where the `Lazy` points to, while the rest
/// use the forward distance from the previous `Lazy`.
/// Distances start at 1, as 0-byte nodes are invalid.
/// Also invalid are nodes being referred in a different
/// order than they were encoded in.
///
/// # Sequences (`Lazy<[T]>`)
///
/// Unlike `Lazy<Vec<T>>`, the length is encoded next to the
/// position, not at the position, which means that the length
/// doesn't need to be known before encoding all the elements.
///
/// If the length is 0, no position is encoded, but otherwise,
/// the encoding is that of `Lazy`, with the distinction that
/// the minimal distance the length of the sequence, i.e.
/// it's assumed there's no 0-byte element in the sequence.
#[must_use]
// FIXME(#59875) the `Meta` parameter only exists to dodge
// invariance wrt `T` (coming from the `meta: T::Meta` field).
pub struct Lazy<T, Meta = <T as LazyMeta>::Meta>
    where T: ?Sized + LazyMeta<Meta = Meta>,
          Meta: 'static + Copy,
{
    pub position: NonZeroUsize,
    pub meta: Meta,
    _marker: PhantomData<T>,
}

impl<T: ?Sized + LazyMeta> Lazy<T> {
    pub fn from_position_and_meta(position: NonZeroUsize, meta: T::Meta) -> Lazy<T> {
        Lazy {
            position,
            meta,
            _marker: PhantomData,
        }
    }
}

impl<T: Encodable> Lazy<T> {
    pub fn from_position(position: NonZeroUsize) -> Lazy<T> {
        Lazy::from_position_and_meta(position, ())
    }
}

impl<T: Encodable> Lazy<[T]> {
    pub fn empty() -> Lazy<[T]> {
        Lazy::from_position_and_meta(NonZeroUsize::new(1).unwrap(), 0)
    }
}

impl<T: ?Sized + LazyMeta> Copy for Lazy<T> {}
impl<T: ?Sized + LazyMeta> Clone for Lazy<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T: ?Sized + LazyMeta> serialize::UseSpecializedEncodable for Lazy<T> {}
impl<T: ?Sized + LazyMeta> serialize::UseSpecializedDecodable for Lazy<T> {}

/// Encoding / decoding state for `Lazy`.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum LazyState {
    /// Outside of a metadata node.
    NoNode,

    /// Inside a metadata node, and before any `Lazy`.
    /// The position is that of the node itself.
    NodeStart(NonZeroUsize),

    /// Inside a metadata node, with a previous `Lazy`.
    /// The position is a conservative estimate of where that
    /// previous `Lazy` would end (see their comments).
    Previous(NonZeroUsize),
}

// FIXME(#59875) `Lazy!(T)` replaces `Lazy<T>`, passing the `Meta` parameter
// manually, instead of relying on the default, to get the correct variance.
// Only needed when `T` itself contains a parameter (e.g. `'tcx`).
macro_rules! Lazy {
    (PerDefTable<$T:ty>) => {Lazy<PerDefTable<$T>, [usize; 2]>};
    ([$T:ty]) => {Lazy<[$T], usize>};
    ($T:ty) => {Lazy<$T, ()>};
}

#[derive(RustcEncodable, RustcDecodable)]
pub struct CrateRoot<'tcx> {
    pub name: Symbol,
    pub triple: TargetTriple,
    pub extra_filename: String,
    pub hash: Svh,
    pub disambiguator: CrateDisambiguator,
    pub panic_strategy: PanicStrategy,
    pub edition: Edition,
    pub has_global_allocator: bool,
    pub has_panic_handler: bool,
    pub has_default_lib_allocator: bool,
    pub plugin_registrar_fn: Option<DefIndex>,
    pub proc_macro_decls_static: Option<DefIndex>,
    pub proc_macro_stability: Option<attr::Stability>,

    pub crate_deps: Lazy<[CrateDep]>,
    pub dylib_dependency_formats: Lazy<[Option<LinkagePreference>]>,
    pub lib_features: Lazy<[(Symbol, Option<Symbol>)]>,
    pub lang_items: Lazy<[(DefIndex, usize)]>,
    pub lang_items_missing: Lazy<[lang_items::LangItem]>,
    pub native_libraries: Lazy<[NativeLibrary]>,
    pub foreign_modules: Lazy<[ForeignModule]>,
    pub source_map: Lazy<[syntax_pos::SourceFile]>,
    pub def_path_table: Lazy<hir::map::definitions::DefPathTable>,
    pub impls: Lazy<[TraitImpls]>,
    pub exported_symbols: Lazy!([(ExportedSymbol<'tcx>, SymbolExportLevel)]),
    pub interpret_alloc_index: Lazy<[u32]>,

    pub per_def: LazyPerDefTables<'tcx>,

    pub compiler_builtins: bool,
    pub needs_allocator: bool,
    pub needs_panic_runtime: bool,
    pub no_builtins: bool,
    pub panic_runtime: bool,
    pub profiler_runtime: bool,
    pub sanitizer_runtime: bool,
}

#[derive(RustcEncodable, RustcDecodable)]
pub struct CrateDep {
    pub name: ast::Name,
    pub hash: Svh,
    pub kind: DepKind,
    pub extra_filename: String,
}

#[derive(RustcEncodable, RustcDecodable)]
pub struct TraitImpls {
    pub trait_id: (u32, DefIndex),
    pub impls: Lazy<[DefIndex]>,
}

#[derive(RustcEncodable, RustcDecodable)]
pub struct LazyPerDefTables<'tcx> {
    pub entry: Lazy!(PerDefTable<Lazy<Entry<'tcx>>>),
}

#[derive(RustcEncodable, RustcDecodable)]
pub struct Entry<'tcx> {
    pub kind: EntryKind<'tcx>,
    pub visibility: Lazy<ty::Visibility>,
    pub span: Lazy<Span>,
    pub attributes: Lazy<[ast::Attribute]>,
    pub children: Lazy<[DefIndex]>,
    pub stability: Option<Lazy<attr::Stability>>,
    pub deprecation: Option<Lazy<attr::Deprecation>>,

    pub ty: Option<Lazy!(Ty<'tcx>)>,
    pub inherent_impls: Lazy<[DefIndex]>,
    pub variances: Lazy<[ty::Variance]>,
    pub generics: Option<Lazy<ty::Generics>>,
    pub predicates: Option<Lazy!(ty::GenericPredicates<'tcx>)>,
    pub predicates_defined_on: Option<Lazy!(ty::GenericPredicates<'tcx>)>,

    pub mir: Option<Lazy!(mir::Mir<'tcx>)>,
}

#[derive(Copy, Clone, RustcEncodable, RustcDecodable)]
pub enum EntryKind<'tcx> {
    Const(ConstQualif, Lazy<RenderedConst>),
    ImmStatic,
    MutStatic,
    ForeignImmStatic,
    ForeignMutStatic,
    ForeignMod,
    ForeignType,
    GlobalAsm,
    Type,
    TypeParam,
    ConstParam,
    Existential,
    Enum(ReprOptions),
    Field,
    Variant(Lazy!(VariantData<'tcx>)),
    Struct(Lazy!(VariantData<'tcx>), ReprOptions),
    Union(Lazy!(VariantData<'tcx>), ReprOptions),
    Fn(Lazy!(FnData<'tcx>)),
    ForeignFn(Lazy!(FnData<'tcx>)),
    Mod(Lazy<ModData>),
    MacroDef(Lazy<MacroDef>),
    Closure(Lazy!(ClosureData<'tcx>)),
    Generator(Lazy!(GeneratorData<'tcx>)),
    Trait(Lazy!(TraitData<'tcx>)),
    Impl(Lazy!(ImplData<'tcx>)),
    Method(Lazy!(MethodData<'tcx>)),
    AssociatedType(AssociatedContainer),
    AssociatedExistential(AssociatedContainer),
    AssociatedConst(AssociatedContainer, ConstQualif, Lazy<RenderedConst>),
    TraitAlias(Lazy!(TraitAliasData<'tcx>)),
}

/// Additional data for EntryKind::Const and EntryKind::AssociatedConst
#[derive(Clone, Copy, RustcEncodable, RustcDecodable)]
pub struct ConstQualif {
    pub mir: u8,
    pub ast_promotable: bool,
}

/// Contains a constant which has been rendered to a String.
/// Used by rustdoc.
#[derive(RustcEncodable, RustcDecodable)]
pub struct RenderedConst(pub String);

#[derive(RustcEncodable, RustcDecodable)]
pub struct ModData {
    pub reexports: Lazy<[def::Export<hir::HirId>]>,
}

#[derive(RustcEncodable, RustcDecodable)]
pub struct MacroDef {
    pub body: String,
    pub legacy: bool,
}

#[derive(RustcEncodable, RustcDecodable)]
pub struct FnData<'tcx> {
    pub constness: hir::Constness,
    pub arg_names: Lazy<[ast::Name]>,
    pub sig: Lazy!(ty::PolyFnSig<'tcx>),
}

#[derive(RustcEncodable, RustcDecodable)]
pub struct VariantData<'tcx> {
    pub ctor_kind: CtorKind,
    pub discr: ty::VariantDiscr,
    /// If this is unit or tuple-variant/struct, then this is the index of the ctor id.
    pub ctor: Option<DefIndex>,
    /// If this is a tuple struct or variant
    /// ctor, this is its "function" signature.
    pub ctor_sig: Option<Lazy!(ty::PolyFnSig<'tcx>)>,
}

#[derive(RustcEncodable, RustcDecodable)]
pub struct TraitData<'tcx> {
    pub unsafety: hir::Unsafety,
    pub paren_sugar: bool,
    pub has_auto_impl: bool,
    pub is_marker: bool,
    pub super_predicates: Lazy!(ty::GenericPredicates<'tcx>),
}

#[derive(RustcEncodable, RustcDecodable)]
pub struct TraitAliasData<'tcx> {
    pub super_predicates: Lazy!(ty::GenericPredicates<'tcx>),
}

#[derive(RustcEncodable, RustcDecodable)]
pub struct ImplData<'tcx> {
    pub polarity: hir::ImplPolarity,
    pub defaultness: hir::Defaultness,
    pub parent_impl: Option<DefId>,

    /// This is `Some` only for impls of `CoerceUnsized`.
    pub coerce_unsized_info: Option<ty::adjustment::CoerceUnsizedInfo>,
    pub trait_ref: Option<Lazy!(ty::TraitRef<'tcx>)>,
}


/// Describes whether the container of an associated item
/// is a trait or an impl and whether, in a trait, it has
/// a default, or an in impl, whether it's marked "default".
#[derive(Copy, Clone, RustcEncodable, RustcDecodable)]
pub enum AssociatedContainer {
    TraitRequired,
    TraitWithDefault,
    ImplDefault,
    ImplFinal,
}

impl AssociatedContainer {
    pub fn with_def_id(&self, def_id: DefId) -> ty::AssociatedItemContainer {
        match *self {
            AssociatedContainer::TraitRequired |
            AssociatedContainer::TraitWithDefault => ty::TraitContainer(def_id),

            AssociatedContainer::ImplDefault |
            AssociatedContainer::ImplFinal => ty::ImplContainer(def_id),
        }
    }

    pub fn defaultness(&self) -> hir::Defaultness {
        match *self {
            AssociatedContainer::TraitRequired => hir::Defaultness::Default {
                has_value: false,
            },

            AssociatedContainer::TraitWithDefault |
            AssociatedContainer::ImplDefault => hir::Defaultness::Default {
                has_value: true,
            },

            AssociatedContainer::ImplFinal => hir::Defaultness::Final,
        }
    }
}

#[derive(RustcEncodable, RustcDecodable)]
pub struct MethodData<'tcx> {
    pub fn_data: FnData<'tcx>,
    pub container: AssociatedContainer,
    pub has_self: bool,
}

#[derive(RustcEncodable, RustcDecodable)]
pub struct ClosureData<'tcx> {
    pub sig: Lazy!(ty::PolyFnSig<'tcx>),
}

#[derive(RustcEncodable, RustcDecodable)]
pub struct GeneratorData<'tcx> {
    pub layout: mir::GeneratorLayout<'tcx>,
}

// Tags used for encoding Spans:
pub const TAG_VALID_SPAN: u8 = 0;
pub const TAG_INVALID_SPAN: u8 = 1;
