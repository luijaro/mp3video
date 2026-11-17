# mp3video

Aplicación en Rust para Windows con **interfaz gráfica nativa** (y soporte por línea de comandos) para:
- Renderizar lotes de `.mp3` o discos completos **`.cue` + `.flac` / `.wav`** con una imagen estática a videos `.mp4` optimizados con FFmpeg.
- Dividir archivos **`.cue` + `.flac`** a pistas `.mp3` individuales con etiquetas ID3.
- Consultar metadatos y compositores en inglés por código de catálogo (**VGMdb / MusicBrainz**).
- Subir automáticamente los videos y crear playlists en **YouTube**.

## Uso con interfaz gráfica (GUI)

Hacé **doble clic en `mp3video.exe`** (o ejecutalo sin argumentos):

1. **Entrada y salida**: elegí o arrastrá una carpeta con `.mp3`, un archivo `.mp3` individual, un `.cue` o un `.flac` junto a su `.cue`, y una imagen estática opcional (`.png`, `.jpg`, `.jpeg`, `.webp`, `.bmp`).
2. **Soporte CUE + FLAC con revisión en pantalla**:
   - Al cargar o arrastrar un `.cue` (o un `.flac` con `.cue` en la misma carpeta), las pistas detectadas se muestran en pantalla con sus tiempos (`NN | MM:SS.CC - MM:SS.CC | Título`).
   - Podés revisar, editar títulos o eliminar líneas antes de procesar.
   - Podés **▶ Iniciar Renderizado** para generar directamente todos los `.mp4` desde el `.flac` sin archivos intermedios, o usar **✂ Extraer MP3s del CUE** para obtener los `.mp3` por separado.
3. **Metadatos del Álbum (VGMdb / MusicBrainz)**:
   - Ingresá el código de catálogo (ej. `SQEX-10001`), un ID de VGMdb o arrastrá un `.json` de VGMdb, y presioná **🔍 Buscar Disco**.
   - Si hay un `.cue` cargado, actualiza los títulos de las pistas en pantalla con los nombres en inglés del álbum encontrado.
4. **Subida a YouTube y guardado automático**:
   - Seleccioná (o arrastrá) tu `client_secret.json` y presioná **🔑 Conectar YouTube** una sola vez.
   - La sesión (`youtube_tokens.json`, incluyendo las credenciales para renovar el token) y toda la configuración de la interfaz (`mp3video_settings.json`) se guardan automáticamente entre inicios.

## Uso por línea de comandos (CLI)

```powershell
# Consultar ficha del disco, compositores y pistas en inglés por código de catálogo
.\mp3video.exe --vgmdb SQEX-10001 --vgmdb-only

# Dividir un CUE + FLAC en archivos MP3 individuales
.\mp3video.exe -i .\album.cue -o .\mp3_separados --split-mp3

# Renderizar directamente un CUE + FLAC a videos MP4 con fondo blur
.\mp3video.exe -i .\album.cue -m .\cover.jpg -o .\videos --fit blur

# Renderizar MP3s con metadatos del catálogo y subir a YouTube creando una playlist
.\mp3video.exe -i .\temas -m .\portada.png -o .\videos --fit blur --vgmdb SQEX-10001 --youtube-upload --youtube-privacy unlisted --youtube-playlist
```

## Compilación desde el código fuente

```powershell
cargo build --release
```
