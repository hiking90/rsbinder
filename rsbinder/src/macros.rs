
#[macro_export]
macro_rules! declare_binder_interface {
    {
        $interface:path[$descriptor:expr] {
            native: $native:ident($on_transact:path),
            proxy: $proxy:ident,
        }
    } => {
        $crate::declare_binder_interface! {
            $interface[$descriptor] {
                native: $native($on_transact),
                proxy: $proxy {},
                stability: $crate::binder_impl::Stability::default(),
            }
        }
    };

    {
        $interface:path[$descriptor:expr] {
            native: $native:ident($on_transact:path),
            proxy: $proxy:ident,
            stability: $stability:expr,
        }
    } => {
        $crate::declare_binder_interface! {
            $interface[$descriptor] {
                native: $native($on_transact),
                proxy: $proxy {},
                stability: $stability,
            }
        }
    };

    {
        $interface:path[$descriptor:expr] {
            native: $native:ident($on_transact:path),
            proxy: $proxy:ident {
                $($fname:ident: $fty:ty = $finit:expr),*
            },
        }
    } => {
        $crate::declare_binder_interface! {
            $interface[$descriptor] {
                native: $native($on_transact),
                proxy: $proxy {
                    $($fname: $fty = $finit),*
                },
                stability: $crate::binder_impl::Stability::default(),
            }
        }
    };

    {
        $interface:path[$descriptor:expr] {
            native: $native:ident($on_transact:path),
            proxy: $proxy:ident {
                $($fname:ident: $fty:ty = $finit:expr),*
            },
            stability: $stability:expr,
        }
    } => {
        $crate::declare_binder_interface! {
            $interface[$descriptor] {
                @doc[concat!("A binder [`Remotable`]($crate::binder_impl::Remotable) that holds an [`", stringify!($interface), "`] object.")]
                native: $native($on_transact),
                @doc[concat!("A binder [`Proxy`]($crate::binder_impl::Proxy) that holds an [`", stringify!($interface), "`] remote interface.")]
                proxy: $proxy {
                    $($fname: $fty = $finit),*
                },
                stability: $stability,
            }
        }
    };

    {
        $interface:path[$descriptor:expr] {
            @doc[$native_doc:expr]
            native: $native:ident($on_transact:path),

            @doc[$proxy_doc:expr]
            proxy: $proxy:ident {
                $($fname:ident: $fty:ty = $finit:expr),*
            },

            stability: $stability:expr,
        }
    } => {
        #[doc = $proxy_doc]
        pub struct $proxy {
            binder: $crate::StrongIBinder,
            handle: $crate::ProxyHandle,
            $($fname: $fty,)*
        }

        impl $crate::Interface for $proxy {
            fn as_binder(&self) -> $crate::StrongIBinder {
                self.binder.clone()
            }

            fn box_clone(&self) -> Box<dyn $crate::Interface> { todo!() }
        }

        impl $crate::Proxy for $proxy
        // where
        //     $proxy: $interface,
        {
            fn descriptor() -> &'static str {
                $descriptor
            }

            fn from_binder(binder: $crate::StrongIBinder) -> $crate::Result<Self> {
                let proxy = binder.as_proxy().ok_or($crate::Error::from($crate::StatusCode::BadValue))?.clone();
                if proxy.descriptor() != Self::descriptor() {
                    Err($crate::StatusCode::BadType.into())
                } else {
                    Ok(Self { binder, handle: proxy, $($fname: $finit),* })
                }
            }
        }

        impl $crate::parcelable::Serialize for dyn $interface
        where
            dyn $interface: $crate::Interface
        {
            fn serialize(&self, parcel: &mut $crate::Parcel) -> $crate::Result<()> {
                let binder = $crate::Interface::as_binder(self);
                parcel.write(&binder)?;
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{Interface, TransactionCode, Result};

    pub trait IServiceManager: Interface {
        // remote methods...
    }

    declare_binder_interface! {
        IServiceManager["android.os.IServiceManager"] {
            native: BnServiceManager(on_transact),
            proxy: BpServiceManager{},
        }
    }

    fn on_transact(
        service: &dyn IServiceManager,
        code: TransactionCode,
        // data: &BorrowedParcel,
        // reply: &mut BorrowedParcel,
    ) -> Result<()> {
        // ...
        Ok(())
    }

}
