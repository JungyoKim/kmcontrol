//! 크로스-디바이스 zero-copy 핸드오프: WGC 캡처 디바이스(A)와 별도 인코드 디바이스(B)를
//! keyed-mutex 공유 텍스처로 잇는다. A/B가 별도 D3D11 디바이스라 `SetMultithreadProtected`
//! 하의 디바이스 락 경합(WGC vs VideoProcessor vs mfx)이 사라진다 — Sunshine 방식.
//!
//! 흐름: 캡처 콜백(A) 이 WGC BGRA 프레임을 공유 텍스처에 CopyResource 후 keyed mutex 로 넘긴다.
//! 인코드 스레드(B) 가 공유 텍스처를 로컬로 복사(즉시 반납) → 디바이스 B 에서 색변환 + QSV 인코딩.

use anyhow::{anyhow, bail, Result};
use windows::core::{Interface, PCWSTR};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11Device1, ID3D11DeviceContext, ID3D11Multithread,
    ID3D11Texture2D, D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
    D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX, D3D11_RESOURCE_MISC_SHARED_NTHANDLE, D3D11_SDK_VERSION,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter, IDXGIDevice, IDXGIFactory1, IDXGIKeyedMutex,
    IDXGIKeyedMutex_Vtbl, IDXGIResource1,
};

/// AcquireSync 래퍼. windows-rs 의 Result 래퍼는 WAIT_TIMEOUT(0x102)을 양수 HRESULT →
/// 성공으로 매핑해 타임아웃을 구분 못 하므로, raw vtable 호출로 HRESULT 를 직접 본다.
/// 반환: 획득=Ok(true), 타임아웃=Ok(false), 그 외=Err.
pub fn acquire_sync(mutex: &IDXGIKeyedMutex, key: u64, ms: u32) -> Result<bool> {
    unsafe {
        let raw = mutex.as_raw();
        let vtbl = *(raw as *const *const IDXGIKeyedMutex_Vtbl);
        let hr = ((*vtbl).AcquireSync)(raw, key, ms);
        match hr.0 {
            0 => Ok(true),
            0x102 => Ok(false), // WAIT_TIMEOUT
            o => bail!("AcquireSync failed: 0x{o:08x}"),
        }
    }
}
/// keyed-mutex 공유 텍스처 핸드오프 프로토콜 키. 캡처(생산자)가 0 으로 획득→쓰고 1 로 반납,
/// 인코드(소비자)가 1 로 획득→읽고 0 으로 반납. 초기 키 0 이라 캡처가 먼저 진행.
pub const KEY_CAPTURE: u64 = 0;
pub const KEY_ENCODE: u64 = 1;

/// CreateSharedHandle 접근 플래그: DXGI_SHARED_RESOURCE_READ(0x80000000) | _WRITE(0x1).
const DXGI_SHARED_RW: u32 = 0x8000_0001;

/// 디바이스 A 의 어댑터 LUID 를 i64 로 패킹(스레드 간 전달용, Send).
pub fn device_luid(device: &ID3D11Device) -> Result<i64> {
    unsafe {
        let dxgi: IDXGIDevice = device.cast()?;
        let adapter = dxgi.GetAdapter()?;
        let desc = adapter.GetDesc()?;
        let l = desc.AdapterLuid;
        Ok(((l.HighPart as i64) << 32) | (l.LowPart as u32 as i64))
    }
}

/// 지정 LUID 어댑터에 별도 D3D11 디바이스(B)를 생성(VIDEO_SUPPORT 포함, 멀티스레드 보호).
pub fn create_device_for_luid(target: i64) -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
        let mut i = 0u32;
        loop {
            let adapter = match factory.EnumAdapters1(i) {
                Ok(a) => a,
                Err(_) => break,
            };
            i += 1;
            let desc = adapter.GetDesc1()?;
            let l = desc.AdapterLuid;
            let packed = ((l.HighPart as i64) << 32) | (l.LowPart as u32 as i64);
            if packed != target {
                continue;
            }
            let adp: IDXGIAdapter = adapter.cast()?;
            let levels = [D3D_FEATURE_LEVEL_11_0];
            let mut dev: Option<ID3D11Device> = None;
            let mut ctx: Option<ID3D11DeviceContext> = None;
            D3D11CreateDevice(
                &adp,
                D3D_DRIVER_TYPE_UNKNOWN,
                Default::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                Some(&levels),
                D3D11_SDK_VERSION,
                Some(&mut dev),
                None,
                Some(&mut ctx),
            )?;
            let d = dev.ok_or_else(|| anyhow!("device B null"))?;
            let c = ctx.ok_or_else(|| anyhow!("context B null"))?;
            if let Ok(mt) = d.cast::<ID3D11Multithread>() {
                let _ = mt.SetMultithreadProtected(true);
            }
            return Ok((d, c));
        }
        bail!("adapter luid {target} not found")
    }
}

/// 공유 BGRA 텍스처(keyed mutex + NT 핸들). 캡처 디바이스 A 에서 생성한다.
pub struct SharedTex {
    pub tex: ID3D11Texture2D,
    pub mutex: IDXGIKeyedMutex,
    pub handle: isize, // NT 핸들(스레드 간 전달용). 인코드 스레드에서 open_shared 로 연다.
}

pub fn create_shared_bgra(device: &ID3D11Device, w: u32, h: u32) -> Result<SharedTex> {
    unsafe {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: (D3D11_RESOURCE_MISC_SHARED_NTHANDLE.0
                | D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX.0) as u32,
        };
        let mut tex: Option<ID3D11Texture2D> = None;
        device.CreateTexture2D(&desc, None, Some(&mut tex))?;
        let tex = tex.ok_or_else(|| anyhow!("shared tex null"))?;
        let mutex: IDXGIKeyedMutex = tex.cast()?;
        let res1: IDXGIResource1 = tex.cast()?;
        let handle = res1.CreateSharedHandle(None, DXGI_SHARED_RW, PCWSTR::null())?;
        Ok(SharedTex { tex, mutex, handle: handle.0 as isize })
    }
}

/// 인코드 디바이스 B 에서 NT 핸들로 공유 텍스처를 연다.
pub fn open_shared(device: &ID3D11Device, handle: isize) -> Result<(ID3D11Texture2D, IDXGIKeyedMutex)> {
    unsafe {
        let dev1: ID3D11Device1 = device.cast()?;
        let tex: ID3D11Texture2D = dev1.OpenSharedResource1(HANDLE(handle as *mut _))?;
        let mutex: IDXGIKeyedMutex = tex.cast()?;
        Ok((tex, mutex))
    }
}

/// 로컬(비공유) BGRA 텍스처. 공유 텍스처를 즉시 복사받아 keyed mutex 를 빨리 반납하기 위한 대상.
pub fn create_bgra(device: &ID3D11Device, w: u32, h: u32) -> Result<ID3D11Texture2D> {
    unsafe {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut tex: Option<ID3D11Texture2D> = None;
        device.CreateTexture2D(&desc, None, Some(&mut tex))?;
        tex.ok_or_else(|| anyhow!("local bgra tex null"))
    }
}
