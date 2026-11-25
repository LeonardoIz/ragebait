<p align="center">
  <img src="docs/logo.png" alt="ragebait" width="640">
</p>

**ragebait** ejecuta el título de Xbox Series de forma nativa en Windows a
partir de **tu propio dump** del juego. El código x86-64 del título corre sin
cambios dentro de nuestro proceso; ragebait le da la plataforma que espera
(servicios del sistema, D3D12.X sobre D3D12, shaders de consola recompilados a
DXIL).

Es un hard fork de [victory-fork](https://github.com/fxceless/victory-fork),
a su vez fork de [VIctory](https://github.com/Marvisak/VIctory).

> **Este repositorio no contiene archivos, código ni assets del juego.**
> Necesitas tu propia copia. Sin relación con el desarrollador ni con el
> editor del juego. Ver [docs/asset-policy.md](docs/asset-policy.md).

## Objetivos

1. **Recursos.** Bajar el uso de CPU, RAM y VRAM: hoy el proceso ocupa unos
   24 GB y la tarjeta va llena, y la mayoría de los cuellos de botella vienen
   de ahí.
2. **Precompilación.** Hacer por adelantado todo lo posible (shaders,
   pipelines, texturas) para que la ejecución en tiempo real cargue menos.
3. **Patches y mods.** Un sistema para modificar el propio juego, no solo la
   capa, y poder optimizar también desde dentro.

## Estado

**Todavía no es jugable.** Punto de partida heredado de victory-fork
(2026-11-25; i7-14700KF, RTX 4070 12 GB, 32 GB RAM):

| Fase | Estado |
|------|--------|
| Arranque, menús, pantalla de inicio | funciona, 30 fps |
| Cinemática inicial | se reproduce completa, 5–8,5 fps |
| Guardado | autoguardado y carga funcionan |
| Primera escena jugable | se renderiza (personajes, cielo, HUD, mando), 8–10 fps |
| Luz en sombra | negra: la iluminación por ray tracing aún no funciona |
| Memoria | ~24 GB de proceso, VRAM llena desde el primer frame |

Mediciones y experimentos anteriores: [docs/victory-fork-log.md](docs/victory-fork-log.md).

## Empezar

Requisitos: Windows 10/11 x64, CPU con AVX2, GPU Direct3D 12 con shader
model 6.6 y mesh shaders, Rust (versión fijada en `rust-toolchain.toml`).
El título necesita varios GiB de commit libre al arrancar; si `crashes.log`
muestra el error 1455, amplía el archivo de paginación.

```powershell
cargo xtask install-hooks                # comprobación de la política de assets en cada commit
copy victory.example.toml victory.toml   # pon tus rutas (fuera del repo)
cargo test --workspace                   # no necesita datos del juego
cargo run -p victory -- config           # comprueba las rutas
```

Precompilar shaders y ejecutar:

```powershell
cargo build --release -p victory
target\release\victory.exe translate --warm all --jobs 16   # sin rutas: el game_dir de victory.toml
target\release\victory.exe run <dir del juego>\<exe del título> --execute
```

La mejor combinación de switches medida hasta ahora está en
[diagnostics/README.md](diagnostics/README.md). Cada switch `VICTORY_*` está
documentado en un comentario donde se lee.

Build de diagnóstico (logs, perfilador, capturas):

```powershell
cargo build --profile diag -p victory --features diagnostics   # target\diag\victory.exe
```

| Variable | Efecto |
|----------|--------|
| `VICTORY_LOG=<target>=debug,...` | logs de depuración |
| `VICTORY_D3D12_DEBUG=1` / `VICTORY_D3D12_DRED=1` | capa de depuración de D3D12 / datos de fallo de GPU |
| `VICTORY_D3D12_WARP=1` | adaptador por software (lento, no puede tumbar el driver) |
| `VICTORY_GPU_TIMING=1` | cada lista medida por separado |
| `VICTORY_SHOT=<carpeta>` | captura de un frame completo |
| `VICTORY_FRAME_LOG=1` | duración y desglose de cada frame |

Controles: mandos XInput. Sin mando, el teclado hace de mando (WASD/IJKL
sticks, flechas cruceta, Enter/Backspace/X/Y botones, Q/E/Z/C gatillos,
Esc/Tab Menú/Vista).

## Ramas

- `main`: desarrollo de ragebait.
- `upstream`: copia exacta de `main` de victory-fork, para traer cambios con
  `git merge upstream`.

## Créditos

- [VIctory](https://github.com/Marvisak/VIctory), de Marvisak y
  colaboradores: el loader, los servicios, el backend D3D12 y el recompilador
  de shaders.
- [victory-fork](https://github.com/fxceless/victory-fork), de fxceless y
  colaboradores, con aportaciones de myokex, Joshua Odom y los autores de los
  pull requests que cita su historial.
- [XCL](https://github.com/ivall/xcl), de ivall, como referencia de
  comportamiento.

## Licencia

GPL-3.0-or-later. Ver [LICENSE](LICENSE).
