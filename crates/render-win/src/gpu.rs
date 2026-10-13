use windows::{
    core::*,
    Win32::{
        Foundation::*,
        Graphics::{Direct2D::*, Direct3D::*, Direct3D11::*, DirectWrite::*, Dxgi::*},
    },
};

#[derive(Clone)]
pub struct Gpu {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    d2d: ID2D1Device,
    dxgi: IDXGIDevice,
    factory: IDXGIFactory2,
    write: IDWriteFactory,
    software: bool,
}
impl Gpu {
    /// D3D11 device, borrowed by the shared-texture producer.
    pub(crate) fn device(&self) -> &ID3D11Device {
        &self.device
    }
    /// Immediate device context, borrowed by the shared-texture producer.
    pub(crate) fn context(&self) -> &ID3D11DeviceContext {
        &self.context
    }
    /// DXGI device, borrowed by the shared-texture producer for the adapter LUID.
    pub(crate) fn dxgi(&self) -> &IDXGIDevice {
        &self.dxgi
    }
    /// Swap-chain factory, borrowed by the composition target builder.
    pub(crate) fn factory(&self) -> &IDXGIFactory2 {
        &self.factory
    }
    /// DirectWrite factory, borrowed for text layouts and formats.
    pub(crate) fn write(&self) -> &IDWriteFactory {
        &self.write
    }
    /// True when the device was created on the software (WARP) adapter.
    pub(crate) fn is_software(&self) -> bool {
        self.software
    }
    pub fn new(adapter: Option<&IDXGIAdapter1>) -> Result<Self> {
        unsafe {
            let mut device = None;
            let mut context = None;
            let flags = D3D11_CREATE_DEVICE_BGRA_SUPPORT;
            let base_adapter: Option<IDXGIAdapter> = adapter.map(|a| a.cast()).transpose()?;
            let attempt = D3D11CreateDevice(
                base_adapter.as_ref(),
                D3D_DRIVER_TYPE(if adapter.is_some() {
                    D3D_DRIVER_TYPE_UNKNOWN.0
                } else {
                    D3D_DRIVER_TYPE_HARDWARE.0
                }),
                HMODULE::default(),
                flags,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            );
            let software = attempt.is_err();
            if software {
                D3D11CreateDevice(
                    None,
                    D3D_DRIVER_TYPE_WARP,
                    HMODULE::default(),
                    flags,
                    None,
                    D3D11_SDK_VERSION,
                    Some(&mut device),
                    None,
                    Some(&mut context),
                )?;
            }
            let device = device.unwrap();
            let context = context.unwrap();
            let dxgi: IDXGIDevice = device.cast()?;
            let d2d_factory: ID2D1Factory1 =
                D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
            let d2d = d2d_factory.CreateDevice(&dxgi)?;
            let adapter = dxgi.GetAdapter()?;
            let factory: IDXGIFactory2 = adapter.GetParent()?;
            let write: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
            Ok(Self {
                device,
                context,
                d2d,
                dxgi,
                factory,
                write,
                software,
            })
        }
    }
    pub fn dc(&self) -> Result<ID2D1DeviceContext> {
        unsafe {
            self.d2d
                .CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE)
        }
    }
}
