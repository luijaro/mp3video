# mp3video

Aplicación en Rust para Windows con **interfaz gráfica nativa** (y soporte por línea de comandos) para:
- Renderizar lotes de archivos de audio (**`.flac`**, **`.mp3`**, **`.wav`**, **`.m4a`**, etc.) o discos completos **`.cue` + `.flac` / `.wav`** con una imagen estática a videos `.mp4` optimizados con FFmpeg.
- Armar **1 solo video largo de álbum completo** (`--full-album` / modo **Álbum completo**) con imagen 100% fija, **capítulos embebidos en el MP4** y **timestamps listos para YouTube** (`00:00 Título`, o `00:00:00 Título` si supera 1 hora), tanto desde `.cue`+`.flac` (1 o varios discos), `.flac` con CUE embebido, como desde carpetas con pistas `.flac` o `.mp3`.
- Generar automáticamente archivos de texto **`{video}.txt`** (con Título y Descripción listos para copiar y pegar) y una lista maestra **`_youtube_tracklist.txt`** en la carpeta de salida en caso de no poder o no querer subir automáticamente a YouTube.
- Dividir archivos **`.cue` + `.flac`** a pistas `.mp3` individuales con etiquetas ID3.
- Consultar metadatos y compositores en inglés por código de catálogo (**VGMdb / MusicBrainz**).
- Subir automáticamente los videos y crear playlists en **YouTube**.

## Uso con interfaz gráfica (GUI)

Hacé **doble clic en `mp3video.exe`** (o ejecutalo sin argumentos):

1. **Entrada y salida**: elegí o arrastrá una carpeta con audios (`.flac`, `.mp3`, etc.) o varios `.cue`, un archivo individual (`.flac`, `.mp3`, etc.), un `.cue` o un `.flac` junto a su `.cue`, y una imagen estática opcional (`.png`, `.jpg`, `.jpeg`, `.webp`, `.bmp`).
2. **Soporte FLAC, CUE y Modo Álbum completo con revisión en pantalla**:
   - Al cargar o arrastrar un archivo `.flac` suelto (con o sin CUE embebido), un `.cue`, o al activar **Álbum completo (1 solo video con timestamps / capítulos)** sobre una carpeta con archivos de audio, las pistas detectadas se muestran en pantalla con sus tiempos (`NN | MM:SS.CC - MM:SS.CC | Título`).
   - Podés revisar, editar títulos o eliminar líneas antes de procesar.
   - En modo normal podés **▶ Iniciar Renderizado** para generar un `.mp4` por pista o **✂ Extraer MP3s del CUE** para obtener los `.mp3` por separado.
   - Con **Álbum completo** activado, genera un único video (`{album} - Full Soundtrack.mp4`) con capítulos embebidos en el MP4 e incluye el tracklist con timestamps en la descripción de YouTube (también podés copiarlos al portapapeles con **📋 Copiar Timestamps**).
3. **Respaldo en TXT para subida manual a YouTube**:
   - Cada video renderizado genera automáticamente un archivo `{video}.txt` con su **Title** y **Description** formateados.
   - Además, se crea un archivo **`_youtube_tracklist.txt`** en la carpeta de salida con todos los videos de la tanda. Si la subida por API falla o no tenés credenciales configuradas, podés copiar y pegar directamente desde estos archivos a YouTube Studio.
4. **Metadatos del Álbum (VGMdb / MusicBrainz)**:
   - Ingresá el código de catálogo (ej. `SQEX-10001`), un ID de VGMdb o arrastrá un `.json` de VGMdb, y presioná **🔍 Buscar Disco**.
   - Actualiza los títulos de las pistas en pantalla con los nombres en inglés del álbum encontrado.
5. **Subida a YouTube y guardado automático**:
   - Seleccioná (o arrastrá) tu `client_secret.json` y presioná **🔑 Conectar YouTube** una sola vez.
   - La sesión (`youtube_tokens.json`, incluyendo las credenciales para renovar el token) y toda la configuración de la interfaz (`mp3video_settings.json`) se guardan automáticamente entre inicios.

## Uso por línea de comandos (CLI)

```powershell
# Renderizar un archivo FLAC suelto a MP4 con fondo blur y generar su TXT descriptivo
.\mp3video.exe -i .\cancion.flac -m .\cover.jpg -o .\videos --fit blur

# Consultar ficha del disco, compositores y pistas en inglés por código de catálogo
.\mp3video.exe --vgmdb SQEX-10001 --vgmdb-only

# Dividir un CUE + FLAC en archivos MP3 individuales
.\mp3video.exe -i .\album.cue -o .\mp3_separados --split-mp3

# Renderizar directamente un CUE + FLAC a videos MP4 individuales con fondo blur
.\mp3video.exe -i .\album.cue -m .\cover.jpg -o .\videos --fit blur

# Armar 1 video largo de álbum completo con capítulos + timestamps desde carpeta de FLACs (o MP3s)
.\mp3video.exe -i .\carpeta_flac -m .\cover.jpg -o .\videos --fit blur --full-album

# Renderizar pistas con metadatos del catálogo y subir a YouTube creando una playlist
.\mp3video.exe -i .\temas -m .\portada.png -o .\videos --fit blur --vgmdb SQEX-10001 --youtube-upload --youtube-privacy unlisted --youtube-playlist
```

## Compilación desde el código fuente

```powershell
cargo build --release
```
