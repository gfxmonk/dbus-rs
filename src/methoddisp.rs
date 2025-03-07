#![allow(dead_code)]

use {MessageItem, Message, MessageType, Connection, ConnectionItem, Error, ErrorName};
use {Signature, Member, Path};
use Interface as IfaceName;
use std::cell::RefCell;
use std::sync::{Arc, Mutex};
use std::collections::BTreeMap;
use std::marker::PhantomData;
use std::ffi::CString;
use std::fmt;

type ArcMap<K, V> = BTreeMap<Arc<K>, Arc<V>>;

#[derive(Clone, Debug, PartialOrd, Ord, PartialEq, Eq)]
pub struct Argument(Option<String>, Signature);

impl Argument {
    pub fn new(name: Option<String>, sig: Signature) -> Argument { Argument(name, sig) }

    fn introspect(&self, indent: &str, dir: &str) -> String { 
        let n = self.0.as_ref().map(|n| format!("name=\"{}\" ", n)).unwrap_or("".into());
        format!("{}<arg {}type=\"{}\"{}/>\n", indent, n, self.1, dir)
    }
    fn introspect_all(args: &[Argument], indent: &str, dir: &str) -> String {
        args.iter().fold("".to_string(), |aa, az| format!("{}{}", aa, az.introspect(indent, dir)))
    }
}

// Doesn't work, conflicting impls
// impl<S: Into<Signature>> From<S> for Argument

impl From<Signature> for Argument {
    fn from(t: Signature) -> Argument { Argument(None, t) }
}

impl<'a> From<&'a str> for Argument {
    fn from(t: &str) -> Argument { Argument(None, t.into()) }
}

impl<N: Into<String>, S: Into<Signature>> From<(N, S)> for Argument {
    fn from((n, s): (N, S)) -> Argument { Argument(Some(n.into()), s.into()) }
}

#[derive(Clone, Debug, PartialOrd, Ord, PartialEq, Eq)]
pub struct MethodErr(ErrorName, String);

impl MethodErr {
    pub fn invalid_arg<T: fmt::Debug>(a: &T) -> MethodErr {
        ("org.freedesktop.DBus.Error.InvalidArgs", format!("Invalid argument {:?}", a)).into()
    }
    pub fn no_arg() -> MethodErr {
        ("org.freedesktop.DBus.Error.InvalidArgs", "Not enough arguments").into()
    }
    pub fn failed<T: fmt::Display>(a: &T) -> MethodErr {
        ("org.freedesktop.DBus.Error.Failed", a.to_string()).into()
    }
    pub fn no_interface<T: fmt::Display>(a: &T) -> MethodErr {
        ("org.freedesktop.DBus.Error.UnknownInterface", format!("Unknown interface {}", a)).into()
    }
    pub fn no_property<T: fmt::Display>(a: &T) -> MethodErr {
        ("org.freedesktop.DBus.Error.UnknownProperty", format!("Unknown property {}", a)).into()
    }
    pub fn ro_property<T: fmt::Display>(a: &T) -> MethodErr {
        ("org.freedesktop.DBus.Error.PropertyReadOnly", format!("Property {} is read only", a)).into()
    }
}

impl<T: Into<ErrorName>, M: Into<String>> From<(T, M)> for MethodErr {
    fn from((t, m): (T, M)) -> MethodErr { MethodErr(t.into(), m.into()) }
}

pub type MethodResult = Result<Vec<Message>, MethodErr>;

struct MethodFn<'a>(Box<Fn(&Message, &ObjectPath<MethodFn<'a>>, &Tree<MethodFn<'a>>) -> MethodResult + 'a>);
struct MethodFnMut<'a>(Box<RefCell<FnMut(&Message, &ObjectPath<MethodFnMut<'a>>, &Tree<MethodFnMut<'a>>) -> MethodResult + 'a>>);
struct MethodSync(Box<Fn(&Message, &ObjectPath<MethodSync>, &Tree<MethodSync>) -> MethodResult + Send + Sync + 'static>);

impl<'a> fmt::Debug for MethodFn<'a> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result { write!(f, "<Fn>") }
}

impl<'a> fmt::Debug for MethodFnMut<'a> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result { write!(f, "<FnMut>") }
}

impl fmt::Debug for MethodSync {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result { write!(f, "<Fn + Send + Sync>") }
}

trait MCall: Sized {
    fn call_method(&self, m: &Message, o: &ObjectPath<Self>, i: &Tree<Self>) -> MethodResult;
    fn box_method<H>(h: H) -> Self
    where H: Fn(&Message, &ObjectPath<Self>, &Tree<Self>) -> MethodResult + Send + Sync + 'static;
}

impl<'a> MCall for MethodFn<'a> {
    fn call_method(&self, m: &Message, o: &ObjectPath<MethodFn<'a>>, i: &Tree<MethodFn<'a>>) -> MethodResult { self.0(m, o, i) }

    fn box_method<H>(h: H) -> Self
    where H: Fn(&Message, &ObjectPath<MethodFn<'a>>, &Tree<MethodFn<'a>>) -> MethodResult + Send + Sync + 'static {
        MethodFn(Box::new(h))
    }
}

impl MCall for MethodSync {
    fn call_method(&self, m: &Message, o: &ObjectPath<MethodSync>, i: &Tree<MethodSync>) -> MethodResult { self.0(m, o, i) }

    fn box_method<H>(h: H) -> Self
    where H: Fn(&Message, &ObjectPath<MethodSync>, &Tree<MethodSync>) -> MethodResult + Send + Sync + 'static {
        MethodSync(Box::new(h))
    }
}

impl<'a> MCall for MethodFnMut<'a> {
    fn call_method(&self, m: &Message, o: &ObjectPath<MethodFnMut<'a>>, i: &Tree<MethodFnMut<'a>>) -> MethodResult {
        let mut z = self.0.borrow_mut();
        (&mut *z)(m, o, i)
    }

    fn box_method<H>(h: H) -> Self
    where H: Fn(&Message, &ObjectPath<MethodFnMut<'a>>, &Tree<MethodFnMut<'a>>) -> MethodResult + Send + Sync + 'static {
        MethodFnMut(Box::new(RefCell::new(h)))
    }
}

#[derive(Debug)]
pub struct Method<M> {
    cb: M,
    name: Arc<Member>,
    i_args: Vec<Argument>,
    o_args: Vec<Argument>,
    anns: BTreeMap<String, String>,
}

impl<M> Method<M> {
    pub fn in_arg<A: Into<Argument>>(mut self, a: A) -> Self { self.i_args.push(a.into()); self }
    pub fn in_args<Z: Into<Argument>, A: IntoIterator<Item=Z>>(mut self, a: A) -> Self {
        self.i_args.extend(a.into_iter().map(|b| b.into())); self
    }

    pub fn out_arg<A: Into<Argument>>(mut self, a: A) -> Self { self.o_args.push(a.into()); self }
    pub fn out_args<Z: Into<Argument>, A: IntoIterator<Item=Z>>(mut self, a: A) -> Self {
        self.o_args.extend(a.into_iter().map(|b| b.into())); self
    }

    /// Add an annotation to the method
    pub fn annotate<N: Into<String>, V: Into<String>>(mut self, name: N, value: V) -> Self {
        self.anns.insert(name.into(), value.into()); self
    }
    /// Add an annotation that this entity is deprecated.
    pub fn deprecated(self) -> Self { self.annotate("org.freedesktop.DBus.Deprecated", "true") }
}

impl<M: MCall> Method<M> {
    pub fn call(&self, m: &Message, o: &ObjectPath<M>, i: &Tree<M>) -> MethodResult { self.cb.call_method(m, o, i) }

    fn new(n: Member, cb: M) -> Self { Method { name: Arc::new(n), i_args: vec!(), o_args: vec!(), anns: BTreeMap::new(), cb: cb } }
}


#[derive(Debug)]
pub struct Interface<M> {
    name: Arc<IfaceName>,
    methods: ArcMap<Member, Method<M>>,
    signals: ArcMap<Member, Signal>,
    properties: ArcMap<String, Property<M>>,
    anns: BTreeMap<String, String>,
}

impl<M> Interface<M> {
    /// Adds a method to the interface.
    pub fn add_m(mut self, m: Method<M>) -> Self { self.methods.insert(m.name.clone(), Arc::new(m)); self }
    /// Adds a signal to the interface.
    pub fn add_s(mut self, s: Signal) -> Self { self.signals.insert(s.name.clone(), Arc::new(s)); self }
    /// Adds a signal to the interface. Returns a reference to the signal
    /// (which you can use to emit the signal, once it belongs to an object path).
    pub fn add_s_ref(&mut self, s: Signal) -> Arc<Signal> {
        let s = Arc::new(s);
        self.signals.insert(s.name.clone(), s.clone());
        s
    }

    /// Adds a signal to the interface.
    pub fn add_p(mut self, p: Property<M>) -> Self { self.properties.insert(p.name.clone(), Arc::new(p)); self }
    /// Adds a property to the interface. Returns a reference to the property
    /// (which you can use to get and set the current value of the property).
    pub fn add_p_ref(&mut self, p: Property<M>) -> Arc<Property<M>> {
        let p = Arc::new(p);
        self.properties.insert(p.name.clone(), p.clone());
        p
    }

    pub fn annotate<N: Into<String>, V: Into<String>>(mut self, name: N, value: V) -> Self {
        self.anns.insert(name.into(), value.into()); self
    }
    /// Add an annotation that this entity is deprecated.
    pub fn deprecated(self) -> Self { self.annotate("org.freedesktop.DBus.Deprecated", "true") }

    fn new(t: IfaceName) -> Interface<M> {
        Interface { name: Arc::new(t), methods: BTreeMap::new(), signals: BTreeMap::new(),
            properties: BTreeMap::new(), anns: BTreeMap::new()
        }
    }

}

#[derive(Copy, Clone, PartialEq, Eq, Ord, PartialOrd, Debug)]
pub enum EmitsChangedSignal {
    True,
    Invalidates,
    Const,
    False,
}

#[derive(Copy, Clone, PartialEq, Eq, Ord, PartialOrd, Debug)]
pub enum Access {
    Read,
    ReadWrite,
    Write,
}

impl Access {
    fn introspect(&self) -> &'static str {
        match self {
            &Access::Read => "read",
            &Access::ReadWrite => "readwrite",
            &Access::Write => "write",
        }
    }
}

#[derive(Debug)]
pub struct Property<M> {
    name: Arc<String>,
    value: Mutex<MessageItem>,
    emits: EmitsChangedSignal,
    rw: Access,
    set_cb: Option<M>,
    owner: Mutex<Option<(Arc<Path>, Arc<IfaceName>)>>,
    anns: BTreeMap<String, String>,
}

impl<M: MCall> Property<M> {
    pub fn get_value(&self) -> MessageItem {
        self.value.lock().unwrap().clone()
    }

    pub fn get_signal(&self) -> Option<Message> {
        self.owner.lock().unwrap().as_ref().map(|&(ref p, ref i)| {
            Message::signal(&p, &"org.freedesktop.DBus.Properties".into(), &"PropertiesChanged".into())
                .append(String::from(&***i))
        })
    }

    /// Returns error if "emits" is "Const", and the property is in a tree.
    /// Returns messages to be sent over a connection, this could be the PropertiesChanged signal.
    pub fn set_value(&self, m: MessageItem) -> Result<Vec<Message>,()> {
        let ss = match self.emits {
            EmitsChangedSignal::False => None,
            EmitsChangedSignal::Const => if self.get_signal().is_some() { return Err(()) } else { None },
            EmitsChangedSignal::True => self.get_signal().map(|mut s| {
                let m = MessageItem::Array(vec!(((&*self.name).clone().into(), Box::new(m.clone()).into()).into()), "{sv}".into());
                s.append_items(&[m]);
                s
            }),
            EmitsChangedSignal::Invalidates => self.get_signal().map(|mut s| {
                let m2 = [(&*self.name).clone()][..].into();
                s.append_items(&[MessageItem::Array(vec!(), "{sv}".into()), m2]);
                s
            }),
        };
        *self.value.lock().unwrap() = m;
        Ok(ss.map(|s| vec!(s)).unwrap_or(vec!()))
    }

    pub fn emits_changed(mut self, e: EmitsChangedSignal) -> Self {
        self.emits = e;
        assert!(self.rw == Access::Read || self.emits != EmitsChangedSignal::Const);
        self
    }

    pub fn access(mut self, e: Access) -> Self {
        self.rw = e;
        assert!(self.rw == Access::Read || self.emits != EmitsChangedSignal::Const);
        self
    }

    pub fn remote_get(&self, _: &Message) -> Result<MessageItem, MethodErr> {
        // TODO: We should be able to call a user-defined callback here instead...
        if self.rw == Access::Write { return Err(MethodErr::failed(&format!("Property {} is write only", &self.name))) }
        Ok(self.get_value())
    }

    /// Helper method to verify and extract a MessageItem from a Set message
    pub fn verify_remote_set(&self, m: &Message) -> Result<MessageItem, MethodErr> {
        let items = m.get_items();
        let s: &MessageItem = try!(items.get(2).ok_or_else(|| MethodErr::no_arg())
            .and_then(|i| i.inner().map_err(|_| MethodErr::invalid_arg(&i))));

        if self.rw == Access::Read { Err(MethodErr::ro_property(&self.name)) }
        else if s.type_sig() != self.value.lock().unwrap().type_sig() {
            Err(MethodErr::failed(&format!("Property {} cannot change type to {}", &self.name, s.type_sig())))
        }
        else { Ok(s.clone()) }
    }

    fn remote_set(&self, m: &Message, o: &ObjectPath<M>, t: &Tree<M>) -> Result<Vec<Message>, MethodErr> {
        if let Some(ref cb) = self.set_cb {
            cb.call_method(m, o, t)
        }
        else {
            let s = try!(self.verify_remote_set(m));
            self.set_value(s).map_err(|_| MethodErr::ro_property(&self.name))
        }
    }

    pub fn annotate<N: Into<String>, V: Into<String>>(mut self, name: N, value: V) -> Self {
        self.anns.insert(name.into(), value.into()); self
    }
    /// Add an annotation that this entity is deprecated.
    pub fn deprecated(self) -> Self { self.annotate("org.freedesktop.DBus.Deprecated", "true") }

    fn new(s: String, i: MessageItem) -> Property<M> {
        Property { name: Arc::new(s), emits: EmitsChangedSignal::True, rw: Access::Read,
            value: Mutex::new(i), owner: Mutex::new(None), anns: BTreeMap::new(), set_cb: None }
    }
}

impl Property<MethodSync> {
    /// Sets a callback to be called when a "Set" call is coming in from the remote side.
    /// Might change to something more ergonomic.
    /// For multi-thread use.
    pub fn on_set<H>(mut self, m: H) -> Self
    where H: Fn(&Message, &ObjectPath<MethodSync>, &Tree<MethodSync>) -> MethodResult + Send + Sync + 'static {
        self.set_cb = Some(MethodSync::box_method(m));
        self
    }
}

impl<'a> Property<MethodFn<'a>> {
    /// Sets a callback to be called when a "Set" call is coming in from the remote side.
    /// Might change to something more ergonomic.
    /// For single-thread use.
    pub fn on_set<H: 'a>(mut self, m: H) -> Self
    where H: Fn(&Message, &ObjectPath<MethodFn<'a>>, &Tree<MethodFn<'a>>) -> MethodResult {
        self.set_cb = Some(MethodFn(Box::new(m)));
        self
    }
}

impl<'a> Property<MethodFnMut<'a>> {
    /// Sets a callback to be called when a "Set" call is coming in from the remote side.
    /// Might change to something more ergonomic.
    /// For single-thread use.
    pub fn on_set<H: 'a>(mut self, m: H) -> Self
    where H: FnMut(&Message, &ObjectPath<MethodFnMut<'a>>, &Tree<MethodFnMut<'a>>) -> MethodResult {
        self.set_cb = Some(MethodFnMut(Box::new(RefCell::new(m))));
        self
    }
}


#[derive(Debug)]
pub struct Signal {
    name: Arc<Member>,
    arguments: Vec<Argument>,
    owner: Mutex<Option<(Arc<Path>, Arc<IfaceName>)>>,
    anns: BTreeMap<String, String>,
}

impl Signal {
    /// Returns a message which emits the signal when sent.
    /// Panics if the signal is not inserted in an object path.
    pub fn emit(&self, items: &[MessageItem]) -> Message {
        let mut m = {
            let lock = self.owner.lock().unwrap();
            let &(ref p, ref i) = lock.as_ref().unwrap();
            Message::signal(p, i, &self.name)
        };
        m.append_items(items);
        m
    }

    pub fn arg<A: Into<Argument>>(mut self, a: A) -> Self { self.arguments.push(a.into()); self }
    pub fn args<Z: Into<Argument>, A: IntoIterator<Item=Z>>(mut self, a: A) -> Self {
        self.arguments.extend(a.into_iter().map(|b| b.into())); self
    }

    pub fn annotate<N: Into<String>, V: Into<String>>(mut self, name: N, value: V) -> Self {
        self.anns.insert(name.into(), value.into()); self
    }
    /// Add an annotation that this entity is deprecated.
    pub fn deprecated(self) -> Self { self.annotate("org.freedesktop.DBus.Deprecated", "true") }
}

fn introspect_anns(anns: &BTreeMap<String, String>, indent: &str) -> String {
    anns.iter().fold("".into(), |aa, (ak, av)| {
        format!("{}{}<annotation name=\"{}\" value=\"{}\"/>\n", aa, indent, ak, av)
    })
}

fn introspect_map<T, I: fmt::Display, C: Fn(&T) -> (String, String)>
    (h: &ArcMap<I, T>, name: &str, indent: &str, func: C) -> String {

    h.iter().fold("".into(), |a, (k, v)| {
        let (params, contents) = func(v);
        format!("{}{}<{} name=\"{}\"{}{}>\n",
            a, indent, name, &**k, params, if contents.len() > 0 {
                format!(">\n{}{}</{}", contents, indent, name)
            }
            else { format!("/") }
        )
    })
}

#[derive(Debug)]
pub struct ObjectPath<M> {
    name: Arc<Path>,
    ifaces: ArcMap<IfaceName, Interface<M>>,
}

impl<M: MCall> ObjectPath<M> {

    fn prop_set(&self, m: &Message, o: &ObjectPath<M>, t: &Tree<M>) -> MethodResult {
        let items = m.get_items();
        let iface_name: &String = try!(items.get(0).ok_or_else(|| MethodErr::no_arg())
            .and_then(|i| i.inner().map_err(|_| MethodErr::invalid_arg(&i))));
        let prop_name: &String = try!(items.get(1).ok_or_else(|| MethodErr::no_arg())
            .and_then(|i| i.inner().map_err(|_| MethodErr::invalid_arg(&i))));
        let iface: &Interface<M> = try!(IfaceName::new(&**iface_name).map_err(|e| MethodErr::invalid_arg(&e))
            .and_then(|i| self.ifaces.get(&i).ok_or_else(|| MethodErr::no_interface(&i))));
        let prop: &Property<M> = try!(iface.properties.get(prop_name).ok_or_else(|| MethodErr::no_property(prop_name)));
        let mut r = try!(prop.remote_set(m, o, t));
        r.push(m.method_return());
        Ok(r)
    }

    fn prop_get(&self, m: &Message) -> MethodResult {
        let items = m.get_items();
        let iface_name: &String = try!(items.get(0).ok_or_else(|| MethodErr::no_arg())
            .and_then(|i| i.inner().map_err(|_| MethodErr::invalid_arg(&i))));
        let prop_name: &String = try!(items.get(1).ok_or_else(|| MethodErr::no_arg())
            .and_then(|i| i.inner().map_err(|_| MethodErr::invalid_arg(&i))));
        let iface: &Interface<M> = try!(IfaceName::new(&**iface_name).map_err(|e| MethodErr::invalid_arg(&e))
            .and_then(|i| self.ifaces.get(&i).ok_or_else(|| MethodErr::no_interface(&i))));
        let prop: &Property<M> = try!(iface.properties.get(prop_name).ok_or_else(|| MethodErr::no_property(prop_name)));
        let r = try!(prop.remote_get(m));
        Ok(vec!(m.method_return().append(Box::new(r))))
    }

    fn prop_get_all(&self, m: &Message) -> MethodResult {
        let items = m.get_items();
        let iface_name: &String = try!(items.get(0).ok_or_else(|| MethodErr::no_arg())
            .and_then(|i| i.inner().map_err(|_| MethodErr::invalid_arg(&i))));
        let iface: &Interface<M> = try!(IfaceName::new(&**iface_name).map_err(|e| MethodErr::invalid_arg(&e))
            .and_then(|i| self.ifaces.get(&i).ok_or_else(|| MethodErr::no_interface(&i))));
        let mut q: Vec<MessageItem> = vec!();
        for v in iface.properties.values() {
             q.push(((&**v.name).into(), try!(v.remote_get(m))).into())
        }
        Ok(vec!(m.method_return().append(MessageItem::Array(q, "{sv}".into()))))
    }

    fn add_property_handler(&mut self) {
        let ifname = IfaceName::from("org.freedesktop.DBus.Properties");
        if self.ifaces.contains_key(&ifname) { return };
        let f: Factory<M> = Factory(PhantomData);
        let i = Interface::<M>::new(ifname)
            .add_m(f.method_sync("Get", |m,o,_| o.prop_get(m) )
                .in_arg(("interface_name", "s")).in_arg(("property_name", "s")).out_arg(("value", "v")))
            .add_m(f.method_sync("GetAll", |m,o,_| o.prop_get_all(m))
                .in_arg(("interface_name", "s")).out_arg(("props", "a{sv}")))
            .add_m(f.method_sync("Set", |m,o,t| o.prop_set(m, o, t))
                .in_args(vec!(("interface_name", "s"), ("property_name", "s"), ("value", "v"))));
        self.ifaces.insert(i.name.clone(), Arc::new(i));
    }

    pub fn add(mut self, p: Interface<M>) -> Self {
        for s in p.signals.values() {
            *s.owner.lock().unwrap() = Some((self.name.clone(), p.name.clone()))
        };
        for s in p.properties.values() {
            *s.owner.lock().unwrap() = Some((self.name.clone(), p.name.clone()))
        };
        if !p.properties.is_empty() { self.add_property_handler(); }
        self.ifaces.insert(p.name.clone(), Arc::new(p));
        self
    }

    /// Adds introspection support for this object path.
    pub fn introspectable(self) -> Self {
        let ifname: IfaceName = "org.freedesktop.DBus.Introspectable".into();
        if self.ifaces.contains_key(&ifname) { return self };
        let f: Factory<M> = Factory(PhantomData);
        self.add(Interface::<M>::new(ifname)
            .add_m(f.method_sync("Introspect",
                |m,o,t| Ok(vec!(m.method_return().append(o.introspect(t)))))
                .out_arg(("xml_data", "s"))))
    }

    fn handle(&self, m: &Message, t: &Tree<M>) -> MethodResult {
        let i = try!(m.interface().and_then(|i| self.ifaces.get(&i)).ok_or(
            ("org.freedesktop.DBus.Error.UnknownInterface", "Unknown interface")));
        let me = try!(m.member().and_then(|me| i.methods.get(&me)).ok_or(
            ("org.freedesktop.DBus.Error.UnknownMethod", "Unknown method")));
        me.call(m, &self, t)
    }

    fn introspect(&self, tree: &Tree<M>) -> String {
        let ifacestr = introspect_map(&self.ifaces, "interface", "  ", |iv|
            (format!(""), format!("{}{}{}{}",
                introspect_map(&iv.methods, "method", "    ", |m| (format!(""), format!("{}{}{}",
                    Argument::introspect_all(&m.i_args, "      ", " direction=\"in\""),
                    Argument::introspect_all(&m.o_args, "      ", " direction=\"out\""),
                    introspect_anns(&m.anns, "      ")
                ))),
                introspect_map(&iv.properties, "property", "    ", |p| (
                    format!(" type=\"{}\" access=\"{}\"", p.get_value().type_sig(), p.rw.introspect()),
                    introspect_anns(&p.anns, "      ")
                )),
                introspect_map(&iv.signals, "signal", "    ", |s| (format!(""), format!("{}{}",
                    Argument::introspect_all(&s.arguments, "      ", ""),
                    introspect_anns(&s.anns, "      ")
                ))),
                introspect_anns(&iv.anns, "    ")
            ))
        );
        let olen = self.name.len()+1;
        let childstr = tree.children(&self, true).iter().fold("".to_string(), |na, n|
            format!("{}  <node name=\"{}\"/>\n", na, &n.name[olen..])
        );

        let nodestr = format!(r##"<!DOCTYPE node PUBLIC "-//freedesktop//DTD D-BUS Object Introspection 1.0//EN" "http://www.freedesktop.org/standards/dbus/1.0/introspect.dtd">
<node name="{}">
{}{}</node>"##, self.name, ifacestr, childstr);
        nodestr
    }

    fn get_managed_objects(&self, t: &Tree<M>) -> MessageItem {
        let mut paths = t.children(&self, false);
        paths.push(&self);
        MessageItem::Array(
            paths.iter().map(|p| ((&**p.name).clone().into(), MessageItem::Array(
                p.ifaces.values().map(|i| ((&**i.name).into(), MessageItem::Array(
                    i.properties.values().map(|pp| ((&**pp.name).into(), Box::new(pp.get_value()
                    ).into()).into()).collect(), "{sv}".into()
                )).into()).collect(), "{sa{sv}}".into()
            )).into()).collect(), "{oa{sa{sv}}}".into()
        )
    }

    /// Adds ObjectManager support for this object path.
    ///
    /// It is not possible to add/remove interfaces while the object path belongs to a tree,
    /// hence no InterfacesAdded / InterfacesRemoved signals are sent.
    pub fn object_manager(self) -> Self {
        let ifname: IfaceName = "org.freedesktop.DBus.ObjectManager".into();
        if self.ifaces.contains_key(&ifname) { return self };
        let f: Factory<M> = Factory(PhantomData);
        self.add(Interface::<M>::new(ifname)
            .add_m(f.method_sync("GetManagedObjects",
                |m,o,t| Ok(vec!(m.method_return().append(o.get_managed_objects(t)))))
                .out_arg("a{oa{sa{sv}}}")))
    }
}

/// An iterator adapter that handles incoming method calls.
///
/// Method calls that match an object path in the tree are handled and consumed by this
/// iterator. Other messages are passed through.
pub struct TreeServer<'a, I, M: 'a> {
    iter: I,
    conn: &'a Connection,
    tree: &'a Tree<M>,
}

impl<'a, I: Iterator<Item=ConnectionItem>, M: 'a + MCall> Iterator for TreeServer<'a, I, M> {
    type Item = ConnectionItem;

    fn next(&mut self) -> Option<ConnectionItem> {
        loop {
            let n = self.iter.next();
            if let &Some(ConnectionItem::MethodCall(ref msg)) = &n {
                if let Some(v) = self.tree.handle(&msg) {
                    // Probably the wisest is to ignore any send errors here -
                    // maybe the remote has disconnected during our processing.
                    for m in v { let _ = self.conn.send(m); };
                    continue;
                }
            }
            return n;
        }
    }
}

/// A collection of object paths.
#[derive(Debug)]
pub struct Tree<M> {
    paths: ArcMap<Path, ObjectPath<M>>
}

impl<M: MCall> Tree<M> {

    fn children(&self, o: &ObjectPath<M>, direct_only: bool) -> Vec<&ObjectPath<M>> {
        let parent: &str = &o.name;
        let plen = parent.len()+1;
        self.paths.values().filter_map(|v| {
            let k: &str = &v.name;
            if !k.starts_with(parent) || k.len() <= plen || &k[plen-1..plen] != "/" {None} else {
                let child = &k[plen..];
                if direct_only && child.contains("/") {None} else {Some(&**v)}
            }
        }).collect()
    }

    pub fn add(mut self, p: ObjectPath<M>) -> Self {
        self.paths.insert(p.name.clone(), Arc::new(p));
        self
    }

    /// Registers or unregisters all object paths in the tree.
    /// FIXME: On error while registering, should unregister the already registered paths.
    pub fn set_registered(&self, c: &Connection, b: bool) -> Result<(), Error> {
        for p in self.paths.keys() {
            if b { try!(c.register_object_path(p)); }
            else { c.unregister_object_path(p); }
        }
        Ok(())
    }

    /// Handles a message. Will return None in case the object path was not
    /// found, or otherwise a list of messages to be sent back.
    pub fn handle(&self, m: &Message) -> Option<Vec<Message>> {
        if m.msg_type() != MessageType::MethodCall { None }
        else { m.path().and_then(|p| self.paths.get(&p).map(|s| s.handle(m, &self)
            .unwrap_or_else(|e| vec!(m.error(&e.0, &CString::new(e.1).unwrap()))))) }
    }

    /// This method takes an `ConnectionItem` iterator (you get it from `Connection::iter()`)
    /// and handles all matching items. Non-matching items (e g signals) are passed through.
    pub fn run<'a, I: Iterator<Item=ConnectionItem>>(&'a self, c: &'a Connection, i: I) -> TreeServer<'a, I, M> {
        TreeServer { iter: i, tree: &self, conn: c }
    }
}

/// The factory is used to create object paths, interfaces, methods etc.
///
/// There are three factories:
///  * Fn - all methods are `Fn()`.
///  * FnMut - all methods are `FnMut()`. This means they can mutate their environment,
///    which has the side effect that if you call it recursively, it will RefCell panic.
///  * Sync - all methods are `Fn() + Send + Sync + 'static`. This means that the methods
///    can be called from different threads in parallel.
#[derive(Debug)]
pub struct Factory<M>(PhantomData<M>);

impl<'a> Factory<MethodFn<'a>> {

    /// Creates a new factory for single-thread use.
    pub fn new_fn() -> Self { Factory(PhantomData) }

    /// Creates a new method for single-thread use.
    pub fn method<'b, H: 'b, T>(&self, t: T, handler: H) -> Method<MethodFn<'b>>
    where H: Fn(&Message, &ObjectPath<MethodFn<'b>>, &Tree<MethodFn<'b>>) -> MethodResult, T: Into<Member> {
        Method::new(t.into(), MethodFn(Box::new(handler)))
    }

    pub fn property<'b, T: Into<String>, I: Into<MessageItem>>(&self, t: T, i: I) -> Property<MethodFn<'b>> {
        Property::new(t.into(), i.into())
    }

    pub fn interface<'b, T: Into<IfaceName>>(&self, t: T) -> Interface<MethodFn<'b>> { Interface::new(t.into()) }
}

impl<'a> Factory<MethodFnMut<'a>> {

    /// Creates a new factory for single-thread + mutable fns use.
    pub fn new_fnmut() -> Self { Factory(PhantomData) }

    /// Creates a new method for single-thread use.
    /// This method can mutate its environment, so it will panic in case
    /// it is called recursively.
    pub fn method<'b, H: 'b, T>(&self, t: T, handler: H) -> Method<MethodFnMut<'b>>
    where H: FnMut(&Message, &ObjectPath<MethodFnMut<'b>>, &Tree<MethodFnMut<'b>>) -> MethodResult, T: Into<Member> {
        Method::new(t.into(), MethodFnMut(Box::new(RefCell::new(handler))))
    }

    pub fn property<'b, T: Into<String>, I: Into<MessageItem>>(&self, t: T, i: I) -> Property<MethodFnMut<'b>> {
        Property::new(t.into(), i.into())
    }

    pub fn interface<'b, T: Into<IfaceName>>(&self, t: T) -> Interface<MethodFnMut<'b>> { Interface::new(t.into()) }
}

impl Factory<MethodSync> {
    
    /// Creates a new factory for multi-thread use.
    /// Trees created will be able to Send and Sync, i e,
    /// it can handle several messages in parallel.
    pub fn new_sync() -> Self { Factory(PhantomData) }

    /// Creates a new method for multi-thread use.
    /// This puts bounds on the callback to enable it to be called from several threads
    /// in parallel.
    pub fn method<H, T>(&self, t: T, handler: H) -> Method<MethodSync>
    where H: Fn(&Message, &ObjectPath<MethodSync>, &Tree<MethodSync>) -> MethodResult + Send + Sync + 'static, T: Into<Member> {
        Method::new(t.into(), MethodSync(Box::new(handler)))
    }

    pub fn property<T: Into<String>, I: Into<MessageItem>>(&self, t: T, i: I) -> Property<MethodSync> {
        Property::new(t.into(), i.into())
    }

    pub fn interface<T: Into<IfaceName>>(&self, t: T) -> Interface<MethodSync> { Interface::new(t.into()) }
}

impl<M> Factory<M> {

    pub fn tree(&self) -> Tree<M> { Tree { paths: BTreeMap::new() }}

    pub fn object_path<T: Into<Path>>(&self, t: T) -> ObjectPath<M> {
        ObjectPath { name: Arc::new(t.into()), ifaces: BTreeMap::new() }
    }

    pub fn signal<T: Into<Member>>(&self, t: T) -> Signal {
        Signal { name: Arc::new(t.into()), arguments: vec!(), owner: Mutex::new(None), anns: BTreeMap::new() }
    }
}

impl<M: MCall> Factory<M> {
    /// Creates a new method with bounds enough to be used in all trees.
    pub fn method_sync<H, T>(&self, t: T, handler: H) -> Method<M>
    where H: Fn(&Message, &ObjectPath<M>, &Tree<M>) -> MethodResult + Send + Sync + 'static, T: Into<Member> {
        Method::new(t.into(), M::box_method(handler))
    }
}

#[test]
fn factory_test() {
    let f = Factory::new_fn();
    f.interface("com.example.hello").deprecated();
    let b = 5i32;
    f.method("GetSomething", move |m,_,_| Ok(vec!({ let mut z = m.method_return(); z.append_items(&[b.into()]); z})));
    let t = f.tree().add(f.object_path("/funghi").add(f.interface("a.b.c").deprecated()));
    let t = t.add(f.object_path("/ab")).add(f.object_path("/a")).add(f.object_path("/a/b/c")).add(f.object_path("/a/b"));
    assert_eq!(t.children(t.paths.get(&Path::from("/a")).unwrap(), true).len(), 1);
}

#[test]
fn test_sync_prop() {
    let f = Factory::new_sync();
    let mut i = f.interface("com.example.echo");
    let p = i.add_p_ref(f.property("EchoCount", 7i32));
    let tree1 = Arc::new(f.tree().add(f.object_path("/echo").introspectable().add(i)));
    let tree2 = tree1.clone();
    println!("{:#?}", tree2);
    ::std::thread::spawn(move || {
        let r = p.set_value(9i32.into()).unwrap();
        let signal = r.get(0).unwrap();
        assert_eq!(signal.msg_type(), MessageType::Signal);
        let mut msg = Message::new_method_call("com.example.echoserver", "/echo", "com.example", "dummy").unwrap();
        super::message::message_set_serial(&mut msg, 3);
        tree2.handle(&msg);
    });

    let mut msg = Message::new_method_call("com.example.echoserver", "/echo", "org.freedesktop.DBus.Properties", "Get").unwrap()
        .append("com.example.echo").append("EchoCount");
    super::message::message_set_serial(&mut msg, 4);
    let r = tree1.handle(&msg).unwrap();
    let r1 = r.get(0).unwrap();
    let ii = r1.get_items();
    let vv: &MessageItem = ii.get(0).unwrap().inner().unwrap();
    let v: i32 = vv.inner().unwrap();
    assert!(v == 7 || v == 9);
}

#[test]
fn prop_lifetime_simple() {
    let f = Factory::new_fnmut();
    let count;
    let mut i = f.interface("com.example.dbus.rs");
    count = i.add_p_ref(f.property("changes", 0i32));

    let _setme = i.add_p_ref(f.property("setme", 0u8).access(Access::ReadWrite).on_set(|_,_,_| {
        let v: i32 = count.get_value().inner().unwrap();
        count.set_value((v + 1).into()).unwrap();
        Ok(vec!())
    }));
}

#[test]
fn prop_server() {
    let (count, setme): (_, RefCell<Option<Arc<Property<_>>>>);
    setme = RefCell::new(None);
    let f = Factory::new_fnmut();
    let mut i = f.interface("com.example.dbus.rs");
    count = i.add_p_ref(f.property("changes", 0i32));
    *setme.borrow_mut() = Some(i.add_p_ref(f.property("setme", 0u8).access(Access::ReadWrite).on_set(|m,_,_| {
        let ss2 = setme.borrow();
        let ss = ss2.as_ref().unwrap();
        let s = try!(ss.verify_remote_set(m));
        let r = try!(ss.set_value(s).map_err(|_| MethodErr::ro_property(&ss.name)));
        let v: i32 = count.get_value().inner().unwrap();
        count.set_value((v + 1).into()).unwrap();
        Ok(r)
    })));
    let tree = f.tree().add(f.object_path("/example").add(i));

    let mut msg = Message::new_method_call("com.example.dbus.rs", "/example", "org.freedesktop.DBus.Properties", "Get").unwrap()
        .append("com.example.dbus.rs").append("changes");
    super::message::message_set_serial(&mut msg, 10);
    let r = tree.handle(&msg).unwrap();
    let r1 = r.get(0).unwrap();
    let ii = r1.get_items();
    let vv: &MessageItem = ii.get(0).unwrap().inner().unwrap();
    let v: i32 = vv.inner().unwrap();
    assert_eq!(v, 0);

    // Read-only
    let mut msg = Message::new_method_call("com.example.dbus.rs", "/example", "org.freedesktop.DBus.Properties", "Set").unwrap()
        .append("com.example.dbus.rs").append("changes").append(5i32);
    super::message::message_set_serial(&mut msg, 20);
    let mut r = tree.handle(&msg).unwrap();
    assert!(r.get_mut(0).unwrap().as_result().is_err());

    // Wrong type
    let mut msg = Message::new_method_call("com.example.dbus.rs", "/example", "org.freedesktop.DBus.Properties", "Set").unwrap()
        .append("com.example.dbus.rs").append("setme").append(8i32);
    super::message::message_set_serial(&mut msg, 30);
    let mut r = tree.handle(&msg).unwrap();
    assert!(r.get_mut(0).unwrap().as_result().is_err());

    // Correct!
    let mut msg = Message::new_method_call("com.example.dbus.rs", "/example", "org.freedesktop.DBus.Properties", "Set").unwrap()
        .append("com.example.dbus.rs").append("setme").append(Box::new(9u8.into()));
    super::message::message_set_serial(&mut msg, 30);
    let mut r = tree.handle(&msg).unwrap();

    println!("{:?}", r[0].as_result());

    let c: i32 = count.get_value().inner().unwrap();
    assert_eq!(c, 1);

}

#[test]
fn test_introspection() {
    let f = Factory::new_sync();
    let t = f.object_path("/echo").introspectable()
        .add(f.interface("com.example.echo")
            .add_m(f.method("Echo", |_,_,_| unimplemented!()).in_arg(("request", "s")).out_arg(("reply", "s")))
            .add_p(f.property("EchoCount", 7i32))
            .add_s(f.signal("Echoed").arg(("data", "s")))
    );

    let actual_result = t.introspect(&f.tree().add(f.object_path("/echo/subpath")));
    println!("\n=== Introspection XML start ===\n{}\n=== Introspection XML end ===", actual_result);

    let expected_result = r##"<!DOCTYPE node PUBLIC "-//freedesktop//DTD D-BUS Object Introspection 1.0//EN" "http://www.freedesktop.org/standards/dbus/1.0/introspect.dtd">
<node name="/echo">
  <interface name="com.example.echo">
    <method name="Echo">
      <arg name="request" type="s" direction="in"/>
      <arg name="reply" type="s" direction="out"/>
    </method>
    <property name="EchoCount" type="i" access="read"/>
    <signal name="Echoed">
      <arg name="data" type="s"/>
    </signal>
  </interface>
  <interface name="org.freedesktop.DBus.Introspectable">
    <method name="Introspect">
      <arg name="xml_data" type="s" direction="out"/>
    </method>
  </interface>
  <interface name="org.freedesktop.DBus.Properties">
    <method name="Get">
      <arg name="interface_name" type="s" direction="in"/>
      <arg name="property_name" type="s" direction="in"/>
      <arg name="value" type="v" direction="out"/>
    </method>
    <method name="GetAll">
      <arg name="interface_name" type="s" direction="in"/>
      <arg name="props" type="a{sv}" direction="out"/>
    </method>
    <method name="Set">
      <arg name="interface_name" type="s" direction="in"/>
      <arg name="property_name" type="s" direction="in"/>
      <arg name="value" type="v" direction="in"/>
    </method>
  </interface>
  <node name="subpath"/>
</node>"##;
 
    assert_eq!(expected_result, actual_result);   
}

